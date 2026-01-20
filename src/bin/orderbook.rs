//! Standalone Orderbook Watcher
//!
//! Connects to Polymarket WebSocket and displays real-time orderbook validation.
//! Usage: cargo run --bin orderbook

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, Level};

use prediction_market_arbitrage::{config, polymarket, types::GlobalState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    info!("📚 Polymarket Orderbook Watcher");

    // Load config just for assets
    let config = config::load_config("config.json").unwrap_or_default();

    // Globals
    let state = Arc::new(tokio::sync::RwLock::new(GlobalState::new()));
    let gamma = polymarket::GammaClient::new();

    info!("🔍 Discovering markets...");
    let pairs = gamma.discover_15m_markets().await?;
    info!("✅ Found {} markets", pairs.len());

    {
        let mut s = state.write().await;
        for pair in pairs {
            s.add_pair(pair);
        }
    }

    if state.read().await.market_count() == 0 {
        info!("No markets found. Exiting.");
        return Ok(());
    }

    // Start WS
    let p_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = polymarket::run_ws(p_state).await {
            info!("WS Error: {}", e);
        }
    });

    info!("📡 Connected to WebSocket. Streaming data...");

    // Display Loop
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Simple console clear (ANSI escape)
        print!("\x1B[2J\x1B[1;1H");
        println!("--- Polymarket Orderbook Snapshot ---");

        let s = state.read().await;
        // Show top 5 active markets
        let mut active_count = 0;
        for m in s.markets.iter() {
            if let Some(pair) = &m.pair {
                let (yes_cents, no_cents, yes_size, no_size) = m.poly.load();

                if yes_cents > 0 || no_cents > 0 {
                    println!(
                        "MARKET: {}\n  YES: {}¢ (Size: {:.1})\n  NO : {}¢ (Size: {:.1})\n  SUM: {}¢",
                        pair.pair_id,
                        yes_cents, yes_size as f64 / 100.0,
                        no_cents, no_size as f64 / 100.0,
                        yes_cents + no_cents
                    );
                    println!("-------------------------------------");
                    active_count += 1;
                    if active_count >= 5 {
                        break;
                    }
                }
            }
        }

        if active_count == 0 {
            println!("Waiting for price data...");
        }
    }
}
