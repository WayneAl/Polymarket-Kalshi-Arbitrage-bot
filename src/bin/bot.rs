//! Prediction Market Arbitrage Trading System
//!
//! A high-performance, production-ready arbitrage trading system for cross-platform
//! prediction markets. This system monitors price discrepancies between Kalshi and
//! Polymarket, executing risk-free arbitrage opportunities in real-time.

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{error, info, warn, Level};

// Import from the library crate
use prediction_market_arbitrage::{
    binance_ws, config, polymarket, strategy_0x8dxd, strategy_copy_trade, strategy_gabagool,
    types::{GlobalState, MarketPair, MarketType},
};

use polymarket::Client;

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

struct SimpleTime;
impl tracing_subscriber::fmt::time::FormatTime for SimpleTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Utc::now();
        write!(w, "{}", now.format("%Y-%m-%dT%H:%M:%SZ"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(Level::INFO.into()),
        )
        .with_timer(SimpleTime)
        .init();

    info!("🚀 Prediction Market Arbitrage System v2.1 (Alloy/CLOB-Client)");

    // Check for dry run mode
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);
    if dry_run {
        info!("   Mode: DRY RUN (set DRY_RUN=0 to execute)");
    } else {
        warn!("   Mode: LIVE EXECUTION");
    }

    // Load configuration
    info!("[CONFIG] Loading config.json...");
    let config = config::load_config("config.json").context("Failed to load config.json")?;
    info!("[CONFIG] Loaded {} assets", config.assets.len());

    // Load Polymarket credentials
    dotenvy::dotenv().ok();
    let poly_private_key = std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
    let poly_funder =
        std::env::var("POLY_FUNDER").context("POLY_FUNDER not set (your wallet address)")?;

    // Create new Client (official-based)
    info!("[POLYMARKET] Initializing official CLOB client...");
    let poly_client = Client::new(
        POLY_CLOB_HOST,
        POLYGON_CHAIN_ID,
        &poly_private_key,
        &poly_funder,
    )
    .await?;

    info!("[POLYMARKET] Client ready for {}", &poly_funder[..10]);

    // === MAIN SESSION LOOP ===

    // Globals
    let state = Arc::new(tokio::sync::RwLock::new(GlobalState::new()));

    // 2. Initialize Binance Price Feed (One for all strategies)
    let symbols: Vec<String> = config.assets.iter().map(|a| a.symbol.clone()).collect();
    info!("🚀 Connecting to Binance Stream for {:?}...", symbols);
    let (binance_client, binance_driver) =
        binance_ws::BinanceClient::new(binance_ws::BINANCE_WS_URL.to_string());

    // Spawn Binance (forever)
    tokio::spawn(binance_driver.run());

    // 4. Discovery Setup
    // Use the gamma client from the wrapper
    let gamma_client = poly_client.gamma.clone();

    // Trackers
    let mut poly_ws_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut active_strategies: std::collections::HashMap<u16, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();

    loop {
        // "Use a while init || ... loop" - We integrate this condition into our persistent loop
        if active_strategies.is_empty() || chrono::Local::now().timestamp() % 900 == 0 {
            info!("🔄 Starting Discovery (Time: {})", chrono::Local::now());

            // A. Discovery
            match gamma_client.discover_15m_markets().await {
                // Trackers
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

                // Check against config assets
                let mut matched_asset = None;
                for asset_cfg in &config.assets {
                    for keyword in &asset_cfg.keywords {
                        if desc.contains(keyword) {
                            matched_asset = Some(asset_cfg);
                            break;
                        }
                    }
                    if matched_asset.is_some() {
                        break;
                    }
                }

                if let Some(asset_cfg) = matched_asset {
                    info!(
                        "✨ Spawning Strategy for {} ({})",
                        pair.pair_id, asset_cfg.symbol
                    );

                    let strategy_type = config
                        .active_strategy
                        .clone()
                        .unwrap_or(crate::config::StrategyType::StrategyGabagool);

                    let handle = match strategy_type {
                        crate::config::StrategyType::Strategy0x8dxd => {
                            let strat = strategy_0x8dxd::Strategy0x8dxd::new(
                                state.clone(),
                                market_id,
                                asset_cfg.symbol.clone(),
                                asset_cfg.default_sigma,
                                poly_client.clone(),
                                binance_client.clone(),
                            )
                            .await;
                            tokio::spawn(strat.run(dry_run))
                        }
                        crate::config::StrategyType::CopyTrade => {
                            let target = config
                                .copy_trade
                                .as_ref()
                                .map(|c| c.target_address.clone())
                                .expect("CopyTrade active but config missing");

                            // Refactored to use API polling
                            let strat = strategy_copy_trade::StrategyCopyTrade::new(
                                state.clone(),
                                market_id,
                                target,
                                poly_client.clone(),
                            )
                            .await;
                            tokio::spawn(strat.run(dry_run))
                        }
                        crate::config::StrategyType::StrategyGabagool => {
                            // Read configurable parameters from env
                            let buy_step = std::env::var("GABAGOOL_BUY_STEP")
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(50.0);

                            let pair_cost_threshold = std::env::var("GABAGOOL_PAIR_COST_THRESHOLD")
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.99);

                            let strat = strategy_gabagool::StrategyGabagool::new(
                                state.clone(),
                                poly_client.clone(),
                                buy_step,
                                pair_cost_threshold,
                            )
                            .await;
                            tokio::spawn(strat.run(dry_run))
                        }
                    };

                    active_strategies.insert(market_id, handle);
                }
            }

            // Sleep to ensure we don't spam discovery in the same second
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }

        // Small sleep to prevent busy loop
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

// Remove run_session entirely as its logic is integrated into main
