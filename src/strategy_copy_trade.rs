use alloy::primitives::U256;
use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::polymarket::Client;
use crate::types::GlobalState;

const POLY_DATA_API: &str = "https://data-api.polymarket.com";

#[derive(Debug, Deserialize)]
struct ActivityItem {
    #[serde(rename = "type")]
    activity_type: String, // "TRADE"
    #[allow(dead_code)]
    timestamp: u64,
    side: String, // "BUY" or "SELL"
    size: f64,
    asset: String, // Token ID as string
    #[serde(rename = "transactionHash")]
    transaction_hash: String,
    title: String,
    pub price: Option<f64>,
    #[serde(rename = "usdcSize")]
    pub usdc_size: Option<f64>,
}

pub struct StrategyCopyTrade {
    state: Arc<RwLock<GlobalState>>,
    _market_id: u16,
    target_address: String,
    client: Client,
    http: reqwest::Client,
}

impl StrategyCopyTrade {
    pub async fn new(
        state: Arc<RwLock<GlobalState>>,
        market_id: u16,
        target_address: String,
        client: Client,
    ) -> Self {
        info!(
            "[CopyTrade] Initialized strategy for target: {}",
            target_address
        );

        Self {
            state,
            _market_id: market_id,
            target_address,
            client,
            http: reqwest::Client::new(),
        }
    }

    pub async fn run(self, dry_run: bool) {
        info!(
            "[CopyTrade] Polling Data API for target: {}",
            self.target_address
        );

        let mut last_seen_hash: Option<String> = None;
        let mut is_first_run = true;

        loop {
            match self.fetch_activity().await {
                Ok(activities) => {
                    // Filter for TRADE only
                    let trades: Vec<&ActivityItem> = activities
                        .iter()
                        .filter(|a| a.activity_type == "TRADE")
                        .collect();

                    if !trades.is_empty() {
                        // On first run, just set the latest hash to avoid processing old trades
                        if is_first_run {
                            if let Some(latest) = trades.first() {
                                last_seen_hash = Some(latest.transaction_hash.clone());
                                info!(
                                    "[CopyTrade] Initialized. Latest trade hash: {}",
                                    latest.transaction_hash
                                );
                            }
                            is_first_run = false;
                        } else {
                            // Process new trades (newer than last_seen_hash)
                            let mut new_trades = Vec::new();
                            for trade in trades {
                                if let Some(last) = &last_seen_hash {
                                    if &trade.transaction_hash == last {
                                        break; // Reached known history
                                    }
                                }
                                new_trades.push(trade);
                            }

                            // Reverse to process strictly chronological if multiple new trades
                            new_trades.reverse();

                            for trade in new_trades {
                                self.process_trade(trade, dry_run).await;
                                last_seen_hash = Some(trade.transaction_hash.clone());
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("[CopyTrade] API Poll failed: {}", e);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }

    async fn fetch_activity(&self) -> Result<Vec<ActivityItem>> {
        let url = format!("{}/activity", POLY_DATA_API);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("user", self.target_address.as_str()),
                ("limit", "10"), // Fetch last 10 activities
                ("activity_type", "TRADE"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("API Error: {}", resp.status());
        }

        let activities: Vec<ActivityItem> = resp.json().await?;
        Ok(activities)
    }

    async fn process_trade(&self, trade: &ActivityItem, dry_run: bool) {
        info!(
            "[CopyTrade] 🔔 New Trade Detected: {} {} {} on {} @ {:?} with {:?} USDC",
            trade.side, trade.size, trade.asset, trade.title, trade.price, trade.usdc_size
        );

        let token_id_u256 = U256::from_str_radix(&trade.asset, 10).unwrap_or(U256::ZERO);

        if let Some((_, side, token_to_trade)) = self.identify_token(token_id_u256).await {
            info!(
                "[CopyTrade] 🎯 Matched Market Token! Side: {} (Token: {})",
                side, token_to_trade
            );

            if trade.side == "BUY" {
                // Copy BUY
                if !dry_run {
                    warn!("[CopyTrade] Executing BUY for {}...", token_to_trade);
                    // Match the size or use a config size?
                    let _ = self
                        .client
                        .buy_fak(&token_to_trade, 0.99, trade.size, true)
                        .await;
                }
            } else if trade.side == "SELL" {
                if !dry_run {
                    warn!("[CopyTrade] Executing SELL for {}...", token_to_trade);
                    let _ = self
                        .client
                        .sell_fak(&token_to_trade, 0.01, trade.size, true)
                        .await;
                }
            }
        } else {
            // warn!(
            //     "[CopyTrade] ⚠️ Could not match token ID {} to any known market.",
            //     trade.asset
            // );
        }
    }

    async fn identify_token(&self, token_id_u256: U256) -> Option<(u16, String, String)> {
        let s = self.state.read().await;

        for (idx, market) in s.markets.iter().enumerate() {
            if let Some(pair) = &market.pair {
                let yes_id = U256::from_str_radix(&pair.poly_yes_token, 10).unwrap_or(U256::ZERO);
                let no_id = U256::from_str_radix(&pair.poly_no_token, 10).unwrap_or(U256::ZERO);

                if token_id_u256 == yes_id {
                    return Some((
                        idx as u16,
                        "YES".to_string(),
                        pair.poly_yes_token.to_string(),
                    ));
                }
                if token_id_u256 == no_id {
                    return Some((idx as u16, "NO".to_string(), pair.poly_no_token.to_string()));
                }
            }
        }
        None
    }
}
// Removed tests module as it depends on deleted code
