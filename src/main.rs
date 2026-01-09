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
mod cache;
mod circuit_breaker;
mod config;
mod discovery;
mod execution;
mod kalshi;
mod polymarket;
mod polymarket_clob;
mod position_tracker;
mod strategy_0x8dxd;
mod types;

use anyhow::{Context, Result};
use std::sync::Arc;

use tracing::{error, info, warn};

use cache::TeamCache;
// use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use config::{ARB_THRESHOLD, ENABLED_LEAGUES, WS_RECONNECT_DELAY_SECS};
// use discovery::DiscoveryClient;
// use execution::{create_execution_channel, run_execution_loop, ExecutionEngine};
// use kalshi::{KalshiApiClient, KalshiConfig};
use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
// use position_tracker::{create_position_channel, position_writer_loop, PositionTracker};
use types::{GlobalState, MarketPair, MarketType};

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("prediction_market_arbitrage=info".parse().unwrap()),
        )
        .init();

    info!("🚀 Prediction Market Arbitrage System v2.0");
    info!(
        "   Profit threshold: <{:.1}¢ ({:.1}% minimum profit)",
        ARB_THRESHOLD * 100.0,
        (1.0 - ARB_THRESHOLD) * 100.0
    );
    info!("   Monitored leagues: {:?}", ENABLED_LEAGUES);

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

    // Create async Polymarket client and derive API credentials
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

    // Load neg_risk cache from Python script output
    match poly_async.load_cache(".clob_market_cache.json") {
        Ok(count) => info!("[POLYMARKET] Loaded {} neg_risk entries from cache", count),
        Err(e) => warn!("[POLYMARKET] Could not load neg_risk cache: {}", e),
    }

    info!("[POLYMARKET] Client ready for {}", &poly_funder[..10]);

    // Load team code mapping cache
    let team_cache = TeamCache::load();
    info!("📂 Loaded {} team code mappings", team_cache.len());

    // === MARKET DISCOVERY (Polymarket Only) ===
    info!("🔍 Starting Polymarket-only discovery for 'Bitcoin'...");
    let gamma_client = crate::polymarket::GammaClient::new();
    let markets = gamma_client.fetch_markets("bitcoin").await?;

    let mut pairs = Vec::new();
    for market in markets {
        if let Some(clob_ids_str) = market.clob_token_ids {
            if let Ok(tokens) = serde_json::from_str::<Vec<String>>(&clob_ids_str) {
                if tokens.len() >= 2 {
                    let slug = market.slug.unwrap_or_else(|| "unknown".to_string());
                    let question = market.question;

                    // Synthesize a MarketPair
                    let pair = MarketPair {
                        pair_id: Arc::from(format!("poly-{}", slug)),
                        league: Arc::from("crypto".to_string()),
                        market_type: MarketType::Moneyline, // Defaulting type
                        description: Arc::from(question),
                        kalshi_event_ticker: Arc::from("NONE"), // Placeholder
                        kalshi_market_ticker: Arc::from("NONE"), // Placeholder
                        poly_slug: Arc::from(slug),
                        poly_yes_token: Arc::from(tokens[0].clone()),
                        poly_no_token: Arc::from(tokens[1].clone()),
                        line_value: None,
                        team_suffix: None,
                    };
                    pairs.push(pair);
                }
            }
        }
    }

    info!("📊 Market discovery complete:");
    info!("   - Found {} Bitcoin markets on Polymarket", pairs.len());

    if pairs.is_empty() {
        error!("No Bitcoin markets found on Polymarket!");
        return Ok(());
    }

    // Build global state
    let state = Arc::new({
        let mut s = GlobalState::new();
        for pair in pairs {
            s.add_pair(pair);
        }
        info!(
            "📡 Global state initialized: tracking {} markets",
            s.market_count()
        );
        s
    });

    // Initialize Binance + 0x8dxd Strategy
    info!("🚀 Enabling 0x8dxd Latency Arbitrage Strategy (Polymarket Only)");

    // Binance WS for price feed
    let (binance_client, price_rx) = crate::binance_ws::BinanceClient::new();
    let binance_handle = tokio::spawn(binance_client.run());

    // 0x8dxd Strategy Engine
    let strat =
        crate::strategy_0x8dxd::Strategy0x8dxd::new(state.clone(), poly_async.clone(), price_rx);
    let strat_handle = tokio::spawn(strat.run(dry_run));

    // Create dummy channel for Polymarket WS execution (since we don't use the main engine)
    let (exec_tx, _exec_rx) = tokio::sync::mpsc::channel(1000);

    // Polymarket WS for orderbook updates (required for state update)
    let poly_state = state.clone();
    let poly_threshold_dummy = 0; // Not used for threshold checks in this mode logic, just passing through
    let poly_handle = tokio::spawn(async move {
        loop {
            if let Err(e) =
                crate::polymarket::run_ws(poly_state.clone(), exec_tx.clone(), poly_threshold_dummy)
                    .await
            {
                error!(
                    "[POLYMARKET] WebSocket disconnected: {} - reconnecting...",
                    e
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // Run until termination
    let _ = tokio::join!(poly_handle, binance_handle, strat_handle);

    Ok(())
}
