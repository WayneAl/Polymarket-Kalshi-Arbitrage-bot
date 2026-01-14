use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Instant;

const VOLATILITY_WINDOW_SECS: u64 = 1800; // 30 mins

// Combined stream for BTC, ETH, SOL, XRP
pub const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker/solusdt@bookTicker/xrpusdt@bookTicker";

#[derive(Debug, Clone, Deserialize)]
pub struct CombinedStreamEvent {
    pub stream: String,
    pub data: BookTickerEvent,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BookTickerEvent {
    #[serde(rename = "u")]
    pub update_id: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "b")]
    pub best_bid: String,
    #[serde(rename = "B")]
    pub best_bid_qty: String,
    #[serde(rename = "a")]
    pub best_ask: String,
    #[serde(rename = "A")]
    pub best_ask_qty: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BinancePrice {
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
    pub timestamp: u64,
}

// Shared handle for strategies
#[derive(Clone)]
pub struct BinanceClient {
    // Asset -> Broadcast Sender (e.g. "BTC" -> Sender)
    senders: Arc<RwLock<HashMap<String, broadcast::Sender<BinancePrice>>>>,
    // Asset -> Price History
    history: Arc<RwLock<HashMap<String, VecDeque<(Instant, f64)>>>>,
}

// Driver that maintains connection
pub struct BinanceDriver {
    client: BinanceClient,
    url: String,
}

impl BinanceClient {
    pub fn new(url: String) -> (Self, BinanceDriver) {
        let client = Self {
            senders: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(HashMap::new())),
        };
        let driver = BinanceDriver {
            client: client.clone(),
            url,
        };
        (client, driver)
    }

    pub fn subscribe(&self, asset: &str) -> broadcast::Receiver<BinancePrice> {
        let asset = asset.to_uppercase();
        let mut senders = self.senders.write().unwrap();
        if let Some(tx) = senders.get(&asset) {
            tx.subscribe()
        } else {
            let (tx, rx) = broadcast::channel(100);
            senders.insert(asset, tx);
            rx
        }
    }

    pub fn get_iv(&self, asset: &str) -> Option<f64> {
        let asset = asset.to_uppercase();
        let history = self.history.read().unwrap();
        let queue = history.get(&asset)?;

        if queue.len() < 10 {
            return None;
        }

        // Calculate IV
        let mut log_returns = Vec::new();
        let mut data_iter = queue.iter();
        let mut prev_price = data_iter.next()?.1;

        for &(_, price) in data_iter {
            let log_ret = (price / prev_price).ln();
            log_returns.push(log_ret);
            prev_price = price;
        }

        if log_returns.is_empty() {
            return None;
        }

        let n = log_returns.len() as f64;
        let mean = log_returns.iter().sum::<f64>() / n;
        let variance = log_returns.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std_dev = variance.sqrt();

        let samples_per_year: f64 = 365.0 * 24.0 * 3600.0;
        let annualized_vol = std_dev * samples_per_year.sqrt();
        Some(annualized_vol)
    }

    fn update_history(&self, asset: &str, price: f64) {
        let asset = asset.to_uppercase();
        let mut history = self.history.write().unwrap();
        let queue = history.entry(asset).or_insert_with(VecDeque::new);

        let now = Instant::now();
        // Limit updates to once per second
        if let Some(&(last_time, _)) = queue.back() {
            if now.duration_since(last_time).as_secs() < 1 {
                return;
            }
        }

        queue.push_back((now, price));

        // Prune old
        while let Some(&(time, _)) = queue.front() {
            if now.duration_since(time).as_secs() > VOLATILITY_WINDOW_SECS {
                queue.pop_front();
            } else {
                break;
            }
        }
    }
}

impl BinanceDriver {
    pub async fn run(self) {
        loop {
            let res = self.connect_and_stream().await;
            if let Err(e) = res {
                error!("[BINANCE] Connection error: {}. Retrying in 5s...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }

    async fn connect_and_stream(&self) -> Result<()> {
        info!("[BINANCE] Connecting to {}", self.url);
        let (ws_stream, _) = connect_async(&self.url).await?;
        info!("[BINANCE] Connected");

        let (mut write, mut read) = ws_stream.split();

        // Spawn Ping Task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                if write.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        });

        while let Some(message) = read.next().await {
            match message? {
                Message::Text(text) => match serde_json::from_str::<CombinedStreamEvent>(&text) {
                    Ok(event) => self.process_ticker(event.data),
                    Err(_) => match serde_json::from_str::<BookTickerEvent>(&text) {
                        Ok(ticker) => self.process_ticker(ticker),
                        Err(e) => warn!("[BINANCE] Parse error: {}", e),
                    },
                },
                Message::Close(_) => return Ok(()),
                _ => {}
            }
        }
        Ok(())
    }

    fn process_ticker(&self, event: BookTickerEvent) {
        if let (Ok(bid), Ok(ask)) = (event.best_bid.parse::<f64>(), event.best_ask.parse::<f64>()) {
            let mid = (bid + ask) / 2.0;

            // Extract asset from symbol (e.g. BTCUSDT -> BTC)
            // Assuming USDT pairs
            let symbol = event.symbol.to_uppercase();
            let asset = symbol.replace("USDT", ""); // Simple hack suitable for explicit list

            let price = BinancePrice {
                symbol: asset.clone(), // Send "BTC" not "BTCUSDT" for cleaner filtering? Or keep full symbol.
                // User said "subscribe to corresponding asset".
                // Strategy holds "BTC".
                // Let's store by "BTC".
                bid,
                ask,
                mid,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };

            // Update History
            self.client.update_history(&asset, mid);

            // Broadcast
            let senders = self.client.senders.read().unwrap();
            if let Some(tx) = senders.get(&asset) {
                let _ = tx.send(price);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_live_binance_connection() {
        // 1. Setup Client with Real URL
        let url = String::from(BINANCE_WS_URL);
        let (client, driver) = BinanceClient::new(url);

        // 2. Run Client in background
        tokio::spawn(async move {
            driver.run().await;
        });

        // 3. Verification - Read messages
        // Subscribe to BTC
        let mut rx = client.subscribe("BTC");

        let mut symbols_seen = std::collections::HashSet::new();
        let start = std::time::Instant::now();

        while start.elapsed() < Duration::from_secs(10) && symbols_seen.len() < 1 {
            if let Ok(price) = rx.recv().await {
                println!("Received Update: {:?}", price);
                assert!(price.bid > 0.0);
                assert!(price.ask > 0.0);
                symbols_seen.insert(price.symbol);
            }
        }

        if symbols_seen.is_empty() {
            panic!("No symbols seen in 10s");
        }
    }
}
