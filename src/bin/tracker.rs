//! Standalone Trade Tracker
//!
//! Monitors a target address for new trades on Polymarket.
//! Usage: cargo run --bin tracker -- <address>

use anyhow::Result;
use clap::Parser;
use prediction_market_arbitrage::strategy_copy_trade::ActivityItem;
use std::time::Duration;
use tracing::{info, warn};

const POLY_DATA_API: &str = "https://data-api.polymarket.com";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Target wallet address to monitor
    #[arg(short, long)]
    target: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let args = Args::parse();
    info!("🕵️ Tracker started for: {}", args.target);

    let client = reqwest::Client::new();
    let mut last_hash: Option<String> = None;
    let mut is_first = true;

    loop {
        let url = format!("{}/activity", POLY_DATA_API);
        match client
            .get(&url)
            .query(&[
                ("user", args.target.as_str()),
                ("limit", "5"),
                ("activity_type", "TRADE"),
            ])
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(activities) = resp.json::<Vec<ActivityItem>>().await {
                    let mut new_trades = Vec::new();

                    if is_first {
                        if let Some(first) = activities.first() {
                            last_hash = Some(first.transaction_hash.clone());
                            info!(
                                "✅ Initialized. Last trade: {} ({})",
                                first.title, first.side
                            );
                        }
                        is_first = false;
                    } else {
                        for act in &activities {
                            if let Some(last) = &last_hash {
                                if &act.transaction_hash == last {
                                    break;
                                }
                            }
                            new_trades.push(act);
                        }
                    }

                    for trade in new_trades.iter().rev() {
                        info!(
                            "🔔 NEW TRADE: {} {} {} @ {:.2} ({} USDC) - {}",
                            trade.side,
                            trade.size,
                            trade.title,
                            trade.price.unwrap_or(0.0),
                            trade.usdc_size.unwrap_or(0.0),
                            trade.transaction_hash
                        );
                        last_hash = Some(trade.transaction_hash.clone());
                    }
                }
            }
            Err(e) => warn!("API Error: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
