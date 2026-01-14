//! Prediction Market Arbitrage Trading System
//!
//! A high-performance, production-ready arbitrage trading system for cross-platform
//! prediction markets. This system monitors price discrepancies between Kalshi and
//! Polymarket, executing risk-free arbitrage opportunities in real-time.
//!
//! ## Strategy
//!
//! The core arbitrage strategy exploits the fundamental property of prediction markets:
//! YES + NO = $1.00 (guaranteed). Arbitrage opportunities exist when:
//!
//! ```
//! Best YES ask (Platform A) + Best NO ask (Platform B) < $1.00
//! ```
//!
//! ## Architecture
//!
//! - **Real-time price monitoring** via WebSocket connections to both platforms
//! - **Lock-free orderbook cache** using atomic operations for zero-copy updates
//! - **SIMD-accelerated arbitrage detection** for sub-millisecond latency
//! - **Concurrent order execution** with automatic position reconciliation
//! - **Circuit breaker protection** with configurable risk limits
//! - **Market discovery system** with intelligent caching and incremental updates

mod binance_ws;
mod config;
mod polymarket;
mod polymarket_clob;
mod strategy_0x8dxd;
mod types;

use anyhow::{Context, Result};
use std::sync::Arc;

use tracing::{error, info, warn, Level};

use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
use types::GlobalState;

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(Level::INFO.into()),
        )
        .init();

    info!("🚀 Prediction Market Arbitrage System v2.0");

    // Check for dry run mode
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);
    if dry_run {
        info!("   Mode: DRY RUN (set DRY_RUN=0 to execute)");
    } else {
        warn!("   Mode: LIVE EXECUTION");
    }

    // Load Polymarket credentials
    dotenvy::dotenv().ok();
    let poly_private_key = std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
    let poly_funder =
        std::env::var("POLY_FUNDER").context("POLY_FUNDER not set (your wallet address)")?;

    // Create async Polymarket client
    info!("[POLYMARKET] Creating async client and deriving API credentials...");
    let poly_async_client = PolymarketAsyncClient::new(
        POLY_CLOB_HOST,
        POLYGON_CHAIN_ID,
        &poly_private_key,
        &poly_funder,
    )?;
    let api_creds = poly_async_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;
    let poly_async = Arc::new(SharedAsyncClient::new(
        poly_async_client,
        prepared_creds,
        POLYGON_CHAIN_ID,
    ));

    info!("[POLYMARKET] Client ready for {}", &poly_funder[..10]);

    // === MAIN SESSION LOOP ===

    // Globals
    let state = Arc::new(tokio::sync::RwLock::new(GlobalState::new()));

    // 2. Initialize Binance Price Feed (One for all strategies)
    // 2. Initialize Binance Price Feed (One loop, shared history)
    info!("🚀 Connecting to Binance Stream for [BTC, ETH, SOL, XRP]...");
    let (binance_client, binance_driver) =
        crate::binance_ws::BinanceClient::new(crate::binance_ws::BINANCE_WS_URL.to_string());

    // Spawn Binance (forever)
    tokio::spawn(binance_driver.run());

    // 4. Discovery Setup
    let gamma_client = crate::polymarket::GammaClient::new();

    // Trackers
    let mut poly_ws_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut active_strategies: std::collections::HashMap<u16, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();

    let mut init = true;

    loop {
        // "Use a while init || ... loop" - We integrate this condition into our persistent loop
        if init || chrono::Local::now().timestamp() % 900 == 0 {
            init = false;
            info!("🔄 Starting Discovery (Time: {})", chrono::Local::now());

            // A. Discovery
            match gamma_client.discover_15m_markets().await {
                Ok(pairs) => {
                    info!("🔎 Discovered {} potential markets", pairs.len());
                    // Add new pairs to global state
                    let mut s = state.write().await;
                    let mut added_any = false;
                    for pair in pairs {
                        if s.add_pair(pair).is_some() {
                            added_any = true;
                        }
                    }

                    // If markets changed, simple restart of PolyWS to pick up new subscriptions
                    // (Or if it's the first run)
                    if added_any || poly_ws_handle.is_none() {
                        if let Some(h) = poly_ws_handle.take() {
                            h.abort();
                        }
                        let p_state = state.clone();
                        poly_ws_handle = Some(tokio::spawn(async move {
                            if let Err(e) = crate::polymarket::run_ws(p_state).await {
                                warn!("PolyWS exited: {}", e);
                            }
                        }));
                    }
                }
                Err(e) => {
                    error!("Discovery failed: {}", e);
                }
            }

            // B. Spawn/Prune Strategies
            // Handle cleanup first
            active_strategies.retain(|id, handle| {
                if handle.is_finished() {
                    info!("Strategy {} finished.", id);
                    false
                } else {
                    true
                }
            });

            let s = state.read().await;
            for (id, market) in s.markets.iter().enumerate() {
                let market_id = id as u16;

                // Skip if not set or already running
                if market.pair.is_none() || active_strategies.contains_key(&market_id) {
                    continue;
                }

                let pair = market.pair.as_ref().unwrap();
                let desc = pair.description.to_lowercase();

                let asset = if desc.contains("bitcoin") {
                    Some("BTC")
                } else if desc.contains("ethereum") {
                    Some("ETH")
                } else if desc.contains("solana") {
                    Some("SOL")
                } else if desc.contains("ripple") || desc.contains("xrp") {
                    Some("XRP")
                } else {
                    None
                };

                if let Some(asset_str) = asset {
                    info!("✨ Spawning Strategy for {} ({})", pair.pair_id, asset_str);

                    let strat = crate::strategy_0x8dxd::Strategy0x8dxd::new(
                        state.clone(),
                        market_id,
                        asset_str.to_string(),
                        poly_async.clone(),
                        binance_client.clone(),
                    )
                    .await;

                    let handle = tokio::spawn(strat.run(dry_run));
                    active_strategies.insert(market_id, handle);
                }
            }

            // Sleep to ensure we don't spam discovery in the same second
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }

        // Small sleep to prevent busy loop
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

// Remove run_session entirely as its logic is integrated into main
