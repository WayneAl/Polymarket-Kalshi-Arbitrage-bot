use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use prediction_market_arbitrage::config;
use prediction_market_arbitrage::polymarket_clob::{
    PolymarketAsyncClient, PreparedCreds, SharedAsyncClient,
};
use prediction_market_arbitrage::strategy_copy_trade::StrategyCopyTrade;
use prediction_market_arbitrage::strategy_copy_trade_ws::StrategyCopyTradeWS;
use prediction_market_arbitrage::types::GlobalState;

#[tokio::main]
async fn main() -> Result<()> {
    // Setup logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    info!("Starting Speed Comparison Tool: WS vs API...");

    // Load Config
    // If config.json fails, try config.toml fallback (though main.rs only tries json)
    let config = config::load_config("config.json")
        .or_else(|_| config::load_config("config.toml"))
        .context("Failed to load config file")?;

    // Determine target address
    let target_address = if let Some(ref ct_config) = config.copy_trade {
        ct_config.target_address.clone()
    } else {
        "0x63ce342161250d705dc0b16df89036c8e5f9ba9a".to_string()
    };

    info!("Target Address: {}", target_address);

    // Load Credentials from Env
    dotenvy::dotenv().ok();
    let poly_private_key = std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
    let poly_funder = std::env::var("POLY_FUNDER").context("POLY_FUNDER not set")?;

    // Create Client
    info!("Creating Polymarket Client...");
    // 137 for Polygon
    let poly_client = PolymarketAsyncClient::new(
        "https://clob.polymarket.com",
        137,
        &poly_private_key,
        &poly_funder,
    )?;

    // Derive Creds (like main.rs)
    let api_creds = poly_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;

    let shared_client = Arc::new(SharedAsyncClient::new(poly_client, prepared_creds, 137));
    let state = Arc::new(RwLock::new(GlobalState::default()));

    // Instantiate API Strategy
    let api_strategy = StrategyCopyTrade::new(
        state.clone(),
        0,
        target_address.clone(),
        shared_client.clone(),
    )
    .await;

    // Instantiate WS Strategy
    let ws_url = std::env::var("POLYGON_WS_URL")
        .or_else(|_| {
            config
                .polygon_ws_url
                .clone()
                .ok_or(std::env::VarError::NotPresent)
        })
        .unwrap_or_default();

    if ws_url.trim().is_empty() {
        panic!("POLYGON_WS_URL is not set in .env (and polygon_ws_url not in config)!");
    }

    let ws_strategy = StrategyCopyTradeWS::new(
        state.clone(),
        0,
        target_address.clone(),
        shared_client.clone(),
        ws_url,
    )
    .await;

    info!("Launching both strategies in parallel...");
    info!("Observe the console for `SPEED_COMPARE_...` logs.");

    // Run both
    tokio::select! {
        _ = api_strategy.run(true) => {
            info!("API Strategy exited");
        }
        _ = ws_strategy.run(true) => {
             info!("WS Strategy exited");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down");
        }
    }

    Ok(())
}
