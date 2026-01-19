use anyhow::{Context, Result};
use prediction_market_arbitrage::polymarket_clob::{
    PolymarketAsyncClient, PreparedCreds, SharedAsyncClient,
};
use prediction_market_arbitrage::strategy_copy_trade_ws::StrategyCopyTradeWS;
use prediction_market_arbitrage::types::GlobalState;
use std::sync::Arc;
use tracing::{info, Level};

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

    info!("🚀 Strategy Copy Trade WS Runner");

    // Load credentials
    dotenvy::dotenv().ok();
    let poly_private_key = std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
    let poly_funder = std::env::var("POLY_FUNDER").context("POLY_FUNDER not set")?;
    let target_address = std::env::var("TARGET_ADDRESS").context("TARGET_ADDRESS not set")?;
    let ws_url = std::env::var("POLYGON_WS_URL").context("POLYGON_WS_URL not set")?;
    // Fallback to WS URL if RPC URL is not set (many providers support both on same URL, or user can set distinct)
    let rpc_url = std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| ws_url.clone());

    // Create Async Client
    info!("[POLYMARKET] Creating async client...");
    let poly_async_client = PolymarketAsyncClient::new(
        POLY_CLOB_HOST,
        &rpc_url,
        POLYGON_CHAIN_ID,
        &poly_private_key,
        &poly_funder,
    )?;

    // Derive credentials using nonce=0 (standard)
    let api_creds = poly_async_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;

    let poly_async = Arc::new(SharedAsyncClient::new(
        poly_async_client,
        prepared_creds,
        POLYGON_CHAIN_ID,
    ));

    info!("[POLYMARKET] Client ready.");

    // Initialize State
    let state = Arc::new(tokio::sync::RwLock::new(GlobalState::new()));

    // Initialize Strategy
    // Using default market_id = 0 since we might be listening to all events or not using ID logic
    let market_id = 0;

    let strategy = StrategyCopyTradeWS::new(
        state.clone(),
        market_id,
        target_address.clone(),
        poly_async.clone(),
        ws_url.clone(),
    )
    .await;

    // Run Strategy
    // Set dry_run based on env, default to true
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);
    info!(
        "Starting StrategyCopyTradeWS (Dry Run: {}) for Target: {}",
        dry_run, target_address
    );

    // Spawn background task to auto-redeem positions every 10 minutes
    {
        let client = poly_async.clone();
        tokio::spawn(async move {
            loop {
                tracing::info!("[AutoRedeemTask] Running auto_redeem_positions...");
                match client.auto_redeem_positions().await {
                    Ok(_) => tracing::info!("[AutoRedeemTask] auto_redeem_positions completed"),
                    Err(e) => {
                        tracing::error!("[AutoRedeemTask] auto_redeem_positions failed: {:?}", e)
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(600)).await; // 10 minutes
            }
        });
    }

    strategy.run(dry_run).await;

    Ok(())
}
