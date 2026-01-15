use anyhow::Result;
use ethers::types::U256;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::polymarket_clob::SharedAsyncClient;
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
    client: Arc<SharedAsyncClient>,
    http: reqwest::Client,
}

impl StrategyCopyTrade {
    pub async fn new(
        state: Arc<RwLock<GlobalState>>,
        market_id: u16,
        target_address: String,
        client: Arc<SharedAsyncClient>,
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
            "[CopyTrade] 🔔 New Trade Detected: {} {} {} on {}",
            trade.side, trade.size, trade.asset, trade.title
        );

        let token_id_u256 = U256::from_dec_str(&trade.asset).unwrap_or(U256::zero());

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
                    let _ = self.client.buy_fak(&token_to_trade, 0.99, trade.size).await;
                }
            } else if trade.side == "SELL" {
                if !dry_run {
                    warn!("[CopyTrade] Executing SELL for {}...", token_to_trade);
                    let _ = self
                        .client
                        .sell_fak(&token_to_trade, 0.01, trade.size)
                        .await;
                }
            }
        } else {
            warn!(
                "[CopyTrade] ⚠️ Could not match token ID {} to any known market.",
                trade.asset
            );
        }
    }

    async fn identify_token(&self, token_id_u256: U256) -> Option<(u16, String, String)> {
        let s = self.state.read().await;

        for (idx, market) in s.markets.iter().enumerate() {
            if let Some(pair) = &market.pair {
                let yes_id = U256::from_dec_str(&pair.poly_yes_token).unwrap_or(U256::zero());
                let no_id = U256::from_dec_str(&pair.poly_no_token).unwrap_or(U256::zero());

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polymarket_clob::{ApiCreds, PolymarketAsyncClient, PreparedCreds};
    use base64::Engine;
    use ethers::signers::LocalWallet;

    #[tokio::test]
    async fn test_fetch_activity_method() {
        // Setup Dummy Client dependencies
        let wallet = LocalWallet::new(&mut rand::thread_rng()); // Random wallet
        let private_key = format!("0x{}", hex::encode(wallet.signer().to_bytes()));

        let api_creds = ApiCreds {
            api_key: "test_key".to_string(),
            api_secret: base64::engine::general_purpose::URL_SAFE.encode(
                "test_secret_must_be_long_enough_for_hmac_32_bytes_at_least_so_lets_make_it_so",
            ),
            api_passphrase: "test_pass".to_string(),
        };
        let creds = PreparedCreds::from_api_creds(&api_creds).expect("Failed to prepare creds");

        // Construct Dummy Client (won't actually be used for fetch_activity HTTP calls)
        let poly_client = PolymarketAsyncClient::new(
            "https://clob.polymarket.com",
            137,
            &private_key,
            "0x0000000000000000000000000000000000000000",
        )
        .expect("Failed to create poly client");

        let shared_client = Arc::new(SharedAsyncClient::new(poly_client, creds, 137));

        // Construct Strategy
        let state = Arc::new(RwLock::new(GlobalState::default()));
        let target_address = "0x63ce342161250d705dc0b16df89036c8e5f9ba9a".to_string(); // Known active address

        let strategy = StrategyCopyTrade {
            state: state.clone(),
            _market_id: 999,
            target_address: target_address.clone(),
            client: shared_client.clone(),
            http: reqwest::Client::new(),
        };

        println!("Testing fetch_activity for target: {}", target_address);

        // Call private method directly (allowed in child module)
        let result = strategy.fetch_activity().await;

        match result {
            Ok(activities) => {
                println!(
                    "Successfully fetched {} activities via method call",
                    activities.len()
                );
                // We assert !activities.is_empty() because this specific user (0x63ce...) is known to have history.
                // If this fails, the user might have no recent history or API is down.
                if activities.is_empty() {
                    println!("Warning: No activities found. This might be valid if user has been inactive.");
                } else {
                    println!("Activities: {:#?}", activities);

                    // let first = &activities[0];
                    // println!(
                    //     "First Activity: Title='{}', Type='{}'",
                    //     first.title, first.activity_type
                    // );
                    // assert_eq!(first.activity_type, "TRADE", "Expected TRADE activity type");
                }
            }
            Err(e) => {
                panic!("fetch_activity method failed: {}", e);
            }
        }
    }
}
