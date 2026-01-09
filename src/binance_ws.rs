use anyhow::Result;
use futures_util::StreamExt;
use serde::Deserialize;

use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@bookTicker";

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
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
    pub timestamp: u64,
}

pub struct BinanceClient {
    tx: broadcast::Sender<BinancePrice>,
}

impl BinanceClient {
    pub fn new() -> (Self, broadcast::Receiver<BinancePrice>) {
        let (tx, rx) = broadcast::channel(100);
        (Self { tx }, rx)
    }

    pub async fn run(self) {
        loop {
            match self.connect_and_stream().await {
                Ok(_) => warn!("[BINANCE] Connection closed normally, reconnecting..."),
                Err(e) => error!("[BINANCE] Connection error: {}, reconnecting in 5s...", e),
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }

    async fn connect_and_stream(&self) -> Result<()> {
        info!("[BINANCE] Connecting to {}", BINANCE_WS_URL);
        let (ws_stream, _) = connect_async(BINANCE_WS_URL).await?;
        info!("[BINANCE] Connected");

        let (_, mut read) = ws_stream.split();

        while let Some(message) = read.next().await {
            match message? {
                Message::Text(text) => match serde_json::from_str::<BookTickerEvent>(&text) {
                    Ok(event) => {
                        if let (Ok(bid), Ok(ask)) =
                            (event.best_bid.parse::<f64>(), event.best_ask.parse::<f64>())
                        {
                            let price = BinancePrice {
                                bid,
                                ask,
                                mid: (bid + ask) / 2.0,
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64,
                            };
                            let _ = self.tx.send(price);
                        }
                    }
                    Err(e) => error!("[BINANCE] Parse error: {}", e),
                },
                Message::Ping(_) => {} // Auto-handled by tungstentite usually, but good to know
                Message::Pong(_) => {}
                Message::Close(_) => return Ok(()),
                _ => {}
            }
        }

        Ok(())
    }
}
