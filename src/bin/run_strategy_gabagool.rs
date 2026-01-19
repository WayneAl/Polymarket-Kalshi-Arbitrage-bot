use anyhow::Result;
use prediction_market_arbitrage::polymarket_clob::{
    PolymarketAsyncClient, PreparedCreds, SharedAsyncClient,
};
use prediction_market_arbitrage::strategy_gabagool::StrategyGabagool;
use prediction_market_arbitrage::types::GlobalState;
use std::sync::Arc;
use tracing::info;

struct SimpleTime;
impl tracing_subscriber::fmt::time::FormatTime for SimpleTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Utc::now();
        write!(w, "{}", now.format("%Y-%m-%dT%H:%M:%SZ"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Init logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_timer(SimpleTime)
        .init();

    info!("🚀 Strategy Gabagool Runner");

    // Load .env
    dotenvy::dotenv().ok();

    let poly_private_key = std::env::var("POLY_PRIVATE_KEY").expect("POLY_PRIVATE_KEY not set");
    let poly_funder = std::env::var("POLY_FUNDER").expect("POLY_FUNDER not set");
    let ws_url = std::env::var("POLYGON_WS_URL").expect("POLYGON_WS_URL not set");
    let rpc_url = std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| ws_url.clone());

    // Create Async Client
    info!("[POLYMARKET] Creating async client...");
    let poly_async_client = PolymarketAsyncClient::new(
        "https://clob.polymarket.com",
        &rpc_url,
        137,
        &poly_private_key,
        &poly_funder,
    )?;

    // Derive credentials
    let api_creds = poly_async_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;

    let shared_client = Arc::new(SharedAsyncClient::new(
        poly_async_client,
        prepared_creds,
        137,
    ));

    // Global state
    let state = Arc::new(tokio::sync::RwLock::new(GlobalState::new()));

    // Read configurable parameters from env
    let buy_step = std::env::var("GABAGOOL_BUY_STEP")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(50.0);

    let pair_cost_threshold = std::env::var("GABAGOOL_PAIR_COST_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.99);

    // Strategy
    let strategy = StrategyGabagool::new(
        state.clone(),
        shared_client.clone(),
        buy_step,
        pair_cost_threshold,
    )
    .await;

    // Dry run flag
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);
    info!(
        "Starting StrategyGabagool (dry_run={} buy_step={} threshold={})",
        dry_run, buy_step, pair_cost_threshold
    );

    strategy.run(dry_run).await;

    Ok(())
}
