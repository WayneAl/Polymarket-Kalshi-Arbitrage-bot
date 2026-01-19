//! Polymarket platform integration client.
//!
//! This module provides WebSocket client for real-time Polymarket price feeds
//! and REST API client for market discovery via the Gamma API.

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::config::{GAMMA_API_BASE, POLYMARKET_WS_URL, POLY_PING_INTERVAL_SECS};

use crate::types::{fxhash_str, parse_price, GlobalState, MarketPair, MarketType, SizeCents};

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

    /// Look up Polymarket market by slug, return (yes_token, no_token)
    /// Tries both the exact date and next day (timezone handling)
    pub async fn lookup_market(&self, slug: &str) -> Result<Option<(String, String)>> {
        // Try exact slug first
        if let Some(tokens) = self.try_lookup_slug(slug).await? {
            return Ok(Some(tokens));
        }

        // Try with next day (Polymarket may use local time)
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

        // Check if active and not closed
        if market.closed == Some(true) || market.active == Some(false) {
            return Ok(None);
        }

        // Parse clobTokenIds JSON array
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

    /// Discover specifically "15M" markets and parse them into MarketPair objects
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

                        // Fetch strike price if asset and expiry are known
                        if let (Some(asset_symbol), Some(expiry_ms)) = (asset, expiry_timestamp) {
                            let expiry_ts = expiry_ms / 1000;
                            let event_end_time_utc = Utc.timestamp_opt(expiry_ts, 0).unwrap();
                            let event_start_time_utc =
                                event_end_time_utc - chrono::Duration::minutes(15);
                            let event_start_time_str = event_start_time_utc
                                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                            let event_end_time_str = event_end_time_utc
                                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

                            let url = format!(
                                "https://polymarket.com/api/crypto/crypto-price?symbol={}&eventStartTime={}&variant=fifteen&endDate={}",
                                asset_symbol, event_start_time_str, event_end_time_str,
                            );

                            info!("[GAMMA] Fetching strike price for {}: {}", slug, url);

                            match self.http.get(&url).send().await {
                                Ok(resp) => {
                                    if resp.status().is_success() {
                                        if let Ok(price_data) = resp.json::<Price>().await {
                                            strike_price = price_data.open_price;
                                        } else {
                                            warn!(
                                                "[GAMMA] Failed to parse price JSON for {}",
                                                slug
                                            );
                                        }
                                    } else {
                                        let status = resp.status();
                                        let body = resp
                                            .text()
                                            .await
                                            .unwrap_or_else(|_| "Unknown".to_string());
                                        warn!(
                                            "[GAMMA] Price API returned status {}: {}",
                                            status, body
                                        );
                                    }
                                }
                                Err(e) => warn!("[GAMMA] Failed to fetch price: {}", e),
                            }
                        }
                        if strike_price.is_some() && ts_now < expiry_timestamp.unwrap() {
                            // Synthesize a MarketPair
                            let pair = MarketPair {
                                pair_id: Arc::from(format!("poly-{}", slug)),
                                market_type: MarketType::Moneyline, // Defaulting type
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
        Ok(pairs)
    }
}

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
    // Optional fields for better identification
    pub slug: Option<String>,
    #[serde(rename = "groupItemTitle")]
    pub group_item_title: Option<String>,
    #[serde(rename = "endDateIso")]
    pub end_date_iso: Option<String>,
    #[serde(rename = "endDate")]
    pub end_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Price {
    #[serde(rename = "openPrice")]
    pub open_price: Option<f64>,
    #[serde(rename = "closePrice")]
    pub close_price: Option<f64>,
    pub timestamp: u64,
    pub completed: bool,
    pub incomplete: bool,
    pub cached: bool,
}

/// Increment the date in a Polymarket slug by 1 day
/// e.g., "epl-che-avl-2025-12-08" -> "epl-che-avl-2025-12-09"
fn increment_date_in_slug(slug: &str) -> Option<String> {
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() < 6 {
        return None;
    }

    let year: i32 = parts[3].parse().ok()?;
    let month: u32 = parts[4].parse().ok()?;
    let day: u32 = parts[5].parse().ok()?;

    // Compute next day
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 31,
    };

    let (new_year, new_month, new_day) = if day >= days_in_month {
        if month == 12 {
            (year + 1, 1, 1)
        } else {
            (year, month + 1, 1)
        }
    } else {
        (year, month, day + 1)
    };

    // Rebuild slug with owned strings
    let prefix = parts[..3].join("-");
    let suffix = if parts.len() > 6 {
        format!("-{}", parts[6..].join("-"))
    } else {
        String::new()
    };

    Some(format!(
        "{}-{}-{:02}-{:02}{}",
        prefix, new_year, new_month, new_day, suffix
    ))
}

// =============================================================================
// WebSocket Runner
// =============================================================================

/// Parse size from Polymarket (format: "123.45" dollars)
#[inline(always)]
fn parse_size(s: &str) -> SizeCents {
    // Parse as f64 and convert to cents
    s.parse::<f64>()
        .map(|size| (size * 100.0).round() as SizeCents)
        .unwrap_or(0)
}

/// WebSocket runner
pub async fn run_ws(state: Arc<tokio::sync::RwLock<GlobalState>>) -> Result<()> {
    // Acquire read lock to get tokens
    let tokens: Vec<String> = {
        let s = state.read().await;
        s.markets
            .iter()
            .take(s.market_count())
            .filter_map(|m| m.pair.as_ref())
            .flat_map(|p| [p.poly_yes_token.to_string(), p.poly_no_token.to_string()])
            .collect()
    };

    if tokens.is_empty() {
        info!("[POLY] No markets to monitor, sleeping...");
        tokio::time::sleep(Duration::from_secs(5)).await;
        return Ok(());
    }

    let (ws_stream, _) = connect_async(POLYMARKET_WS_URL)
        .await
        .context("Failed to connect to Polymarket")?;

    info!("[POLY] Connected");

    let (mut write, mut read) = ws_stream.split();

    // Subscribe
    let subscribe_msg = SubscribeCmd {
        assets_ids: tokens.clone(),
        sub_type: "market",
    };

    write
        .send(Message::Text(serde_json::to_string(&subscribe_msg)?))
        .await?;
    info!("[POLY] Subscribed to {} tokens", tokens.len());

    let mut ping_interval = interval(Duration::from_secs(POLY_PING_INTERVAL_SECS));
    let mut last_message = Instant::now();

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                if let Err(e) = write.send(Message::Ping(vec![])).await {
                    error!("[POLY] Failed to send ping: {}", e);
                    break;
                }
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_message = Instant::now();

                        // Try book snapshot first
                        if let Ok(books) = serde_json::from_str::<Vec<BookSnapshot>>(&text) {
                            println!("Received {} book snapshots", books.len());
                            for book in &books {
                                process_book(&state, book).await;
                            }
                        }
                        // Try price change event
                        else if let Ok(event) = serde_json::from_str::<PriceChangeEvent>(&text) {
                            // println!("Received {:?}", event);
                            if event.event_type.as_deref() == Some("price_change") {
                                if let Some(changes) = &event.price_changes {
                                    for change in changes {
                                        // println!("change {:?}", change);
                                        process_price_change(&state, change).await;
                                    }
                                }
                            }
                        }
                        // Log unknown message types at trace level for debugging
                        else {
                            tracing::trace!("[POLY] Unknown WS message: {}...", &text[..text.len().min(100)]);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Close(frame))) => {
                        warn!("[POLY] Server closed: {:?}", frame);
                        break;
                    }
                    Some(Err(e)) => {
                        error!("[POLY] WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        warn!("[POLY] Stream ended");
                        break;
                    }
                    _ => {}
                }
            }
        }

        if last_message.elapsed() > Duration::from_secs(120) {
            warn!("[POLY] Stale connection, reconnecting...");
            break;
        }
    }

    Ok(())
}

/// Process book snapshot
#[inline]
async fn process_book(state: &Arc<tokio::sync::RwLock<GlobalState>>, book: &BookSnapshot) {
    let token_hash = fxhash_str(&book.asset_id);

    // Find best ask (lowest price)
    let (best_ask, ask_size) = book
        .asks
        .iter()
        .filter_map(|l| {
            let price = parse_price(&l.price);
            let size = parse_size(&l.size);
            if price > 0 {
                Some((price, size))
            } else {
                None
            }
        })
        .min_by_key(|(p, _)| *p)
        .unwrap_or((0, 0));

    // Acquire read lock to query maps
    let s = state.read().await;

    // Check if YES token
    if let Some(&market_id) = s.poly_yes_to_id.get(&token_hash) {
        let market = &s.markets[market_id as usize];
        market.poly.update_yes(best_ask, ask_size);
    }
    // Check if NO token
    else if let Some(&market_id) = s.poly_no_to_id.get(&token_hash) {
        let market = &s.markets[market_id as usize];
        market.poly.update_no(best_ask, ask_size);
    }
}

/// Process price change
#[inline]
async fn process_price_change(
    state: &Arc<tokio::sync::RwLock<GlobalState>>,
    change: &PriceChangeItem,
) {
    let price = parse_price(&change.best_ask);
    // println!("Processing price change for {}: {}", change.asset_id, price);

    let token_hash = fxhash_str(&change.asset_id);

    // Lock!
    let s = state.read().await;

    // Check YES token
    if let Some(&market_id) = s.poly_yes_to_id.get(&token_hash) {
        let market = &s.markets[market_id as usize];
        market.poly.update_yes(price, parse_price("0"));
    }
    // Check NO token
    else if let Some(&market_id) = s.poly_no_to_id.get(&token_hash) {
        let market = &s.markets[market_id as usize];
        market.poly.update_no(price, parse_price("0"));
    }
}
