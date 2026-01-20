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
use tokio::time::{interval, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// Official Client Imports
use alloy::primitives::U256;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer; // trait for with_chain_id
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::order_builder::OrderBuilder;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::Client as ClobClient;
use polymarket_client_sdk::clob::Config;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;

use crate::config::{GAMMA_API_BASE, POLYMARKET_WS_URL, POLY_PING_INTERVAL_SECS};
use crate::types::{fxhash_str, parse_price, GlobalState, MarketPair, MarketType, SizeCents};

// === Types ===

#[derive(Debug, Clone)]
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
}

// === WebSocket Message Types ===

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
    // Store funder address
    pub funder: String,
    pub signer: PrivateKeySigner,
    pub chain_id: u64,
}

impl Client {
    pub async fn new(host: &str, chain_id: u64, private_key: &str, funder: &str) -> Result<Self> {
        let signer = PrivateKeySigner::from_str(private_key)
            .map_err(|e| anyhow!("Invalid private key: {}", e))?
            .with_chain_id(Some(chain_id));

        let config = Config::default();
        let clob = ClobClient::new(host, config)?
            .authentication_builder(&signer)
            .authenticate()
            .await?;

        Ok(Self {
            clob: Arc::new(clob),
            gamma: Arc::new(GammaClient::new()),
            funder: funder.to_string(),
            signer,
            chain_id,
        })
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
}

// === Gamma API Client ===

pub struct GammaClient {
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    pub async fn lookup_market(&self, slug: &str) -> Result<Option<(String, String)>> {
        if let Some(tokens) = self.try_lookup_slug(slug).await? {
            return Ok(Some(tokens));
        }
        if let Some(next_day_slug) = increment_date_in_slug(slug) {
            if let Some(tokens) = self.try_lookup_slug(&next_day_slug).await? {
                info!("  📅 Found with next-day slug: {}", next_day_slug);
                return Ok(Some(tokens));
            }
        }
        Ok(None)
    }

    async fn try_lookup_slug(&self, slug: &str) -> Result<Option<(String, String)>> {
        let url = format!("{}/markets?slug={}", GAMMA_API_BASE, slug);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let markets: Vec<GammaMarket> = resp.json().await?;
        if markets.is_empty() {
            return Ok(None);
        }
        let market = &markets[0];
        if market.closed == Some(true) || market.active == Some(false) {
            return Ok(None);
        }
        let token_ids: Vec<String> = market
            .clob_token_ids
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        if token_ids.len() >= 2 {
            Ok(Some((token_ids[0].clone(), token_ids[1].clone())))
        } else {
            Ok(None)
        }
    }

    pub async fn fetch_markets(&self, tag: &str) -> Result<Vec<GammaMarket>> {
        let url = format!(
            "{}/events?tag_slug={}&order=endDate&ascending=true&closed=false&limit=8",
            GAMMA_API_BASE, tag
        );
        info!("[GAMMA] Fetching markets for tag: {}", tag);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Gamma API request failed: {}", resp.status());
        }
        let events: Vec<GammaEvent> = resp.json().await?;
        let markets: Vec<GammaMarket> =
            events.into_iter().flat_map(|event| event.markets).collect();
        info!("[GAMMA] Found {} markets for tag '{}'", markets.len(), tag);
        Ok(markets)
    }

    pub async fn discover_15m_markets(&self) -> Result<Vec<MarketPair>> {
        let markets = self.fetch_markets("15M").await?;
        let ts_now = Utc::now().timestamp();
        let mut pairs = Vec::new();

        for market in markets {
            if let Some(clob_ids_str) = market.clob_token_ids {
                if let Ok(tokens) = serde_json::from_str::<Vec<String>>(&clob_ids_str) {
                    if tokens.len() >= 2 {
                        let slug = market.slug.clone().unwrap_or_else(|| "unknown".to_string());
                        let question = market.question.clone();
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

                        let expiry_timestamp = market
                            .end_date
                            .as_ref()
                            .and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
                            .map(|dt| dt.timestamp_millis());

                        let mut strike_price: Option<f64> = None;

                        // Fetch strike price logic
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
                            match self.http.get(&url).send().await {
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
                                    poly_yes_token: Arc::from(tokens[0].clone()),
                                    poly_no_token: Arc::from(tokens[1].clone()),
                                    strike_price,
                                    expiry_timestamp,
                                };
                                pairs.push(pair);
                            }
                        }
                    }
                }
            }
        }
        Ok(pairs)
    }
}

// === Data Structures ===

#[derive(Debug, Deserialize)]
pub struct GammaEvent {
    pub markets: Vec<GammaMarket>,
}

#[derive(Debug, Deserialize)]
pub struct GammaMarket {
    #[serde(rename = "question")]
    pub question: String,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    #[serde(rename = "active")]
    pub active: Option<bool>,
    #[serde(rename = "closed")]
    pub closed: Option<bool>,
    pub slug: Option<String>,
    #[serde(rename = "endDate")]
    pub end_date: Option<String>,
}

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
    info!("[POLY] Connected");
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
