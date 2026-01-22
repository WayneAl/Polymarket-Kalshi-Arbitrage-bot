//! Polymarket platform integration client.
//!
//! This module provides WebSocket client for real-time Polymarket price feeds,
//! REST API client for market discovery via the Gamma API, and
//! Order execution via the official CLOB client.

use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// Official Client Imports
use alloy::primitives::U256;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer; // trait for with_chain_id
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::Client as ClobClient;
use polymarket_client_sdk::clob::Config as ClobConfig;
use polymarket_client_sdk::gamma::types::request::MarketsRequest;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::rtds::Client as RtdsClient;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;

use crate::config::{GAMMA_API_BASE, POLYMARKET_WS_URL, POLY_PING_INTERVAL_SECS};
use crate::types::{fxhash_str, parse_price, GlobalState, MarketPair, MarketType, SizeCents};
use polymarket_client_sdk::rtds::CryptoPrice;

// === Types ===

#[derive(Debug, Clone)]
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
}

// === WebSocket Message Types ===
// Kept for now, will replace usage in run_ws later
#[derive(Deserialize, Debug)]
pub struct BookSnapshot {
    pub asset_id: String,
    #[allow(dead_code)]
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Deserialize, Debug)]
pub struct PriceLevel {
    pub price: String,
    pub size: String,
}

#[derive(Deserialize, Debug)]
pub struct PriceChangeEvent {
    pub event_type: Option<String>,
    #[serde(default)]
    pub price_changes: Option<Vec<PriceChangeItem>>,
}

#[derive(Deserialize, Debug)]
pub struct PriceChangeItem {
    pub asset_id: String,
    pub price: String,
    pub side: String,
    pub hash: String,
    pub size: String,
    pub best_bid: String,
    pub best_ask: String,
}

#[derive(Serialize)]
struct SubscribeCmd {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    sub_type: &'static str,
}

// === Client Wrapper ===

#[derive(Clone)]
pub struct Client {
    // Official client handles L1/L2 auth and signing
    pub clob: Arc<ClobClient<Authenticated<Normal>>>,
    // We keep GammaClient for discovery
    pub gamma: Arc<GammaClient>,
    // RTDS client for price feeds
    pub rtds: Arc<RtdsClient>,
    // Store funder address
    pub funder: String,
    pub signer: PrivateKeySigner,
    pub chain_id: u64,
    // Broadcast channel for price updates (shared across strategies)
    price_tx: tokio::sync::broadcast::Sender<CryptoPrice>,
    // Task handle for the global price feed
    price_feed_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

impl Client {
    pub async fn new(private_key: &str, funder: &str) -> Result<Self> {
        let signer = PrivateKeySigner::from_str(private_key)
            .map_err(|e| anyhow!("Invalid private key: {}", e))?
            .with_chain_id(Some(POLYGON_CHAIN_ID));

        let config = ClobConfig::default();
        let clob = ClobClient::new(POLY_CLOB_HOST, config)?
            .authentication_builder(&signer)
            .authenticate()
            .await?;

        // Initialize SDK Gamma Client
        // Note: SDK GammaClient::new takes a URL string or we can use default if it had one,
        // but our probe showed it takes strict URL.
        let gamma = GammaClient::new(GAMMA_API_BASE)
            .map_err(|e| anyhow!("Failed to create Gamma client: {:?}", e))?;

        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("Failed to install rustls crypto provider");

        let rtds = RtdsClient::default();

        // Create broadcast channel for price updates (large buffer to avoid dropping messages)
        let (price_tx, _) = tokio::sync::broadcast::channel(1000);

        Ok(Self {
            clob: Arc::new(clob),
            gamma: Arc::new(gamma),
            rtds: Arc::new(rtds),
            funder: funder.to_string(),
            signer,
            chain_id: POLYGON_CHAIN_ID,
            price_tx,
            price_feed_task: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Start the global price feed that broadcasts crypto prices to all strategies
    /// This should be called once at startup, and strategies subscribe via subscribe_prices()
    pub async fn start_price_feed(&self) -> Result<()> {
        let mut task_guard = self.price_feed_task.lock().await;

        // Only start if not already running
        if task_guard.is_some() {
            return Ok(());
        }

        let rtds_client = self.rtds.clone();
        let price_tx = self.price_tx.clone();

        let handle = tokio::spawn(async move {
            // Subscribe to all crypto prices
            match rtds_client.subscribe_crypto_prices(None) {
                Ok(stream) => {
                    let mut price_stream = Box::pin(stream);

                    while let Some(msg) = price_stream.next().await {
                        match msg {
                            Ok(price) => {
                                // Broadcast to all strategies (ignore if no subscribers)
                                let _ = price_tx.send(price);
                            }
                            Err(e) => {
                                warn!("[PRICE_FEED] Stream error: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("[PRICE_FEED] Failed to subscribe: {}", e);
                }
            }
        });

        *task_guard = Some(handle);
        info!("[PRICE_FEED] Global price feed started");
        Ok(())
    }

    /// Subscribe to price updates for a specific symbol
    /// Returns a broadcast receiver that will receive CryptoPrice updates
    pub fn subscribe_prices(&self) -> tokio::sync::broadcast::Receiver<CryptoPrice> {
        self.price_tx.subscribe()
    }

    /// Execute FAK buy order
    pub async fn buy_fak(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        _fee: bool,
    ) -> Result<PolyFillAsync> {
        self.execute_order(token_id, price, size, Side::Buy).await
    }

    /// Execute FAK sell order
    pub async fn sell_fak(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        _fee: bool,
    ) -> Result<PolyFillAsync> {
        self.execute_order(token_id, price, size, Side::Sell).await
    }

    async fn execute_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: Side,
    ) -> Result<PolyFillAsync> {
        let token_id_u256 = U256::from_str(token_id)?;
        let price_dec = Decimal::from_f64(price).ok_or(anyhow!("Invalid price"))?;
        let size_dec = Decimal::from_f64(size).ok_or(anyhow!("Invalid size"))?;

        let order_request = self
            .clob
            .limit_order()
            .token_id(token_id_u256)
            .side(side)
            .price(price_dec)
            .size(size_dec)
            .order_type(OrderType::FAK)
            .build()
            .await?;

        let signed_order = self.clob.sign(&self.signer, order_request).await?;
        let response = self.clob.post_order(signed_order).await?;

        if let Some(error_msg) = &response.error_msg {
            if !error_msg.is_empty() {
                return Err(anyhow!("Order Error: {}", error_msg));
            }
        }

        let order_id = response.order_id.clone();

        // Optimistic fill assumption for FAK
        let filled_size = size;
        let fill_cost = size * price;

        Ok(PolyFillAsync {
            order_id,
            filled_size,
            fill_cost,
        })
    }

    pub async fn discover_15m_markets(&self) -> Result<Vec<MarketPair>> {
        info!("[GAMMA] Fetching markets for tag: 15M");

        // Gamma API search by tag is usually via events endpoint, but let's try strict markets request if possible.
        // We use limit 50 and filtered by closed=false. We will filter for "15min" in question if needed or rely on expiry logic.

        // find recent 15M timestamp
        let ts_now = Utc::now().timestamp();
        let recent_15m = ts_now - ts_now % (15 * 60);
        println!("Recent 15m timestamp: {}", recent_15m);

        let mut req = MarketsRequest::default();
        req.slug = vec![format!("btc-updown-15m-{}", recent_15m)];

        let markets = self
            .gamma
            .markets(&req)
            .await
            .map_err(|e| anyhow!("Gamma API request failed: {:?}", e))?;
        info!("[GAMMA] Found {} markets for tag '15M'", markets.len());

        let ts_now = Utc::now().timestamp();
        let mut pairs = Vec::new();

        for market in markets {
            if let Some(tokens) = market.clob_token_ids {
                // SDK likely returns Vec<U256> or Vec<String>.
                // Based on previous error "found Arc<Uint<256, 4>>" when using it as string, it returns U256.
                // We need to convert to string.
                if tokens.len() >= 2 {
                    let slug = market.slug.clone().unwrap_or_else(|| "unknown".to_string());
                    // Market question is Option<String>
                    let question = market.question.clone().unwrap_or_default();
                    let asset = if question.to_lowercase().contains("bitcoin") {
                        Some("BTC")
                    } else if question.to_lowercase().contains("ethereum") {
                        Some("ETH")
                    } else if question.to_lowercase().contains("solana") {
                        Some("SOL")
                    } else if question.to_lowercase().contains("ripple")
                        || question.to_lowercase().contains("xrp")
                    {
                        Some("XRP")
                    } else {
                        None
                    };

                    let expiry_timestamp = market.end_date.map(|dt| dt.timestamp_millis());

                    let mut strike_price: Option<f64> = None;

                    // Fetch strike price logic (Original Logic Preserved)
                    if let (Some(asset_symbol), Some(expiry_ms)) = (asset, expiry_timestamp) {
                        let expiry_ts = expiry_ms / 1000;
                        let event_end_time_utc = Utc.timestamp_opt(expiry_ts, 0).unwrap();
                        let event_start_time_utc =
                            event_end_time_utc - chrono::Duration::minutes(15);
                        let url = format!(
                            "https://polymarket.com/api/crypto/crypto-price?symbol={}&eventStartTime={}&variant=fifteen&endDate={}",
                            asset_symbol,
                            event_start_time_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                            event_end_time_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        );
                        // Using a one-off reqwest client since we removed the custom GammaClient struct
                        let http = reqwest::Client::new();
                        match http.get(&url).send().await {
                            Ok(resp) => {
                                if resp.status().is_success() {
                                    if let Ok(price_data) = resp.json::<Price>().await {
                                        strike_price = price_data.open_price;
                                    }
                                }
                            }
                            Err(e) => warn!("[GAMMA] Failed to fetch price: {}", e),
                        }
                    }

                    if let Some(exp) = expiry_timestamp {
                        if ts_now < exp {
                            let pair = MarketPair {
                                pair_id: Arc::from(format!("poly-{}", slug)),
                                market_type: MarketType::Moneyline,
                                description: Arc::from(question),
                                poly_slug: Arc::from(slug),
                                poly_yes_token: Arc::from(tokens[0].to_string()),
                                poly_no_token: Arc::from(tokens[1].to_string()),
                                strike_price,
                                expiry_timestamp,
                            };
                            pairs.push(pair);
                        }
                    }
                }
            }
        }
        Ok(pairs)
    }
}

// === Data Structures ===

// Keep Price struct for the manual fetch
#[derive(Debug, Deserialize)]
pub struct Price {
    #[serde(rename = "openPrice")]
    pub open_price: Option<f64>,
}

fn increment_date_in_slug(slug: &str) -> Option<String> {
    // Basic implementation just to satisfy dependency
    Some(slug.to_string())
}

// === WebSocket Runner ===

// === WebSocket Runner ===

#[inline(always)]
fn parse_size(s: &str) -> SizeCents {
    s.parse::<f64>()
        .map(|size| (size * 100.0).round() as SizeCents)
        .unwrap_or(0)
}

pub async fn run_ws(state: Arc<tokio::sync::RwLock<GlobalState>>) -> Result<()> {
    let tokens = {
        let s = state.read().await;
        s.markets
            .iter()
            .take(s.market_count())
            .filter_map(|m| m.pair.as_ref())
            .flat_map(|p| [p.poly_yes_token.to_string(), p.poly_no_token.to_string()])
            .collect::<Vec<_>>()
    };

    if tokens.is_empty() {
        tokio::time::sleep(Duration::from_secs(5)).await;
        return Ok(());
    }

    let (ws_stream, _) = connect_async(POLYMARKET_WS_URL)
        .await
        .context("Failed to connect to Poly WS")?;
    info!("[POLY CLOB WS] Connected");
    let (mut write, mut read) = ws_stream.split();

    let subscribe_msg = SubscribeCmd {
        assets_ids: tokens,
        sub_type: "market",
    };
    write
        .send(Message::Text(serde_json::to_string(&subscribe_msg)?))
        .await?;

    let mut ping_interval = interval(Duration::from_secs(POLY_PING_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = ping_interval.tick() => { write.send(Message::Ping(vec![])).await.ok(); }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                         if let Ok(books) = serde_json::from_str::<Vec<BookSnapshot>>(&text) {
                            for book in &books { process_book(&state, book).await; }
                        } else if let Ok(event) = serde_json::from_str::<PriceChangeEvent>(&text) {
                            if event.event_type.as_deref() == Some("price_change") {
                                if let Some(changes) = &event.price_changes {
                                    for change in changes { process_price_change(&state, change).await; }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn process_book(state: &Arc<tokio::sync::RwLock<GlobalState>>, book: &BookSnapshot) {
    let token_hash = fxhash_str(&book.asset_id);
    let (best_ask, ask_size) = book
        .asks
        .iter()
        .filter_map(|l| {
            let p = parse_price(&l.price);
            let s = parse_size(&l.size);
            if p > 0 {
                Some((p, s))
            } else {
                None
            }
        })
        .min_by_key(|(p, _)| *p)
        .unwrap_or((0, 0));

    let s = state.read().await;
    if let Some(&id) = s.poly_yes_to_id.get(&token_hash) {
        if let Some(m) = s.markets.get(id as usize) {
            m.poly.update_yes(best_ask, ask_size);
        }
    } else if let Some(&id) = s.poly_no_to_id.get(&token_hash) {
        if let Some(m) = s.markets.get(id as usize) {
            m.poly.update_no(best_ask, ask_size);
        }
    }
}

async fn process_price_change(
    state: &Arc<tokio::sync::RwLock<GlobalState>>,
    change: &PriceChangeItem,
) {
    let price = parse_price(&change.best_ask);
    let token_hash = fxhash_str(&change.asset_id);
    let s = state.read().await;
    if let Some(&id) = s.poly_yes_to_id.get(&token_hash) {
        if let Some(m) = s.markets.get(id as usize) {
            m.poly.update_yes(price, parse_price("0"));
        }
    } else if let Some(&id) = s.poly_no_to_id.get(&token_hash) {
        if let Some(m) = s.markets.get(id as usize) {
            m.poly.update_no(price, parse_price("0"));
        }
    }
}
