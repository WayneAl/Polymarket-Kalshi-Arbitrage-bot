use anyhow::Result;
use chrono::{TimeZone, Utc};
use rand::distributions::Distribution;
use rand::thread_rng;
use regex::Regex;
use statrs::distribution::{ContinuousCDF, Normal};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tracing::{error, info};

use crate::binance_ws::BinancePrice;
use crate::polymarket::Price;
use crate::polymarket_clob::SharedAsyncClient;
use crate::types::{AtomicMarketState, GlobalState};

// Configurable parameters
const MIN_SKEW_PROFIT_CENTS: f64 = 2.0; // Minimum edge (cents) to execute
const GAS_FEE_ESTIMATE_CENTS: f64 = 1.0; // Estimate per share if not batched
const MAX_POSITION_USD: f64 = 50.0; // Small size per trade as per 0x8dxd style
const VOLATILITY_WINDOW_SECS: u64 = 1800; // 30 minutes for IV calculation
const RISK_FREE_RATE: f64 = 0.04; // 4% annual risk-free rate

#[derive(Clone, Copy, Debug)]
pub enum PricingModel {
    BlackScholes,
    MonteCarlo,
}

pub struct Strategy0x8dxd {
    state: Arc<GlobalState>,
    client: Arc<SharedAsyncClient>,
    price_rx: broadcast::Receiver<BinancePrice>,
    regex: Regex,
    price_history: VecDeque<(Instant, f64)>, // (Timestamp, Price)
    pricing_model: PricingModel,
}

impl Strategy0x8dxd {
    pub fn new(
        state: Arc<GlobalState>,
        client: Arc<SharedAsyncClient>,
        price_rx: broadcast::Receiver<BinancePrice>,
    ) -> Self {
        // Regex to parse "Bitcoin > $95,000"
        let regex = Regex::new(r"Bitcoin\s*>\s*\$?([\d,]+\.?\d*)").expect("Invalid Regex");

        // Select model from environment or default to Black-Scholes
        let pricing_model = match std::env::var("PRICING_MODEL").unwrap_or_default().as_str() {
            "monte_carlo" | "mc" => PricingModel::MonteCarlo,
            _ => PricingModel::BlackScholes,
        };
        info!(
            "[0x8dxd] Initialized with Pricing Model: {:?}",
            pricing_model
        );

        Self {
            state,
            client,
            price_rx,
            regex,
            price_history: VecDeque::new(),
            pricing_model,
        }
    }

    pub async fn run(mut self, dry_run: bool) {
        info!("Running 0x8dxd Strategy (Latency Arbitrage)");

        while let Ok(price_update) = self.price_rx.recv().await {
            match self.process_tick(&price_update, dry_run).await {
                Ok(_) => {}
                Err(e) => error!("[0x8dxd] Error processing tick: {}", e),
            }
        }
    }

    async fn process_tick(&mut self, binance_price: &BinancePrice, dry_run: bool) -> Result<()> {
        let btc_price = binance_price.mid; // Use mid price
        self.update_price_history(btc_price);

        // Calculate Realized Volatility (Annualized)
        let sigma = self.calculate_iv().unwrap_or(0.5); // Default to 50% IV if insufficient data

        let market_count = self.state.market_count();
        // Iterate through markets
        for i in 0..market_count {
            if let Some(market) = self.state.get_by_id(i as u16) {
                if let Some(pair) = &market.pair {
                    // 1. Filter for "Bitcoin" and "15min" markets
                    if !pair.pair_id.contains("btc") || !pair.pair_id.contains("15m") {
                        continue;
                    }

                    println!("pair: {:?}", pair);

                    // https://polymarket.com/api/crypto/crypto-price?symbol=BTC&eventStartTime=2026-01-12T07:45:00Z&variant=fifteen&endDate=2026-01-12T08:00:00Z
                    let id = pair.pair_id.split("-").collect::<Vec<&str>>();
                    let event_start_time = id.last().unwrap().parse::<i64>().unwrap();
                    let event_start_time_utc = Utc.timestamp_opt(event_start_time, 0).unwrap();
                    let event_start_time_str =
                        event_start_time_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

                    let event_end_time_utc = event_start_time_utc + chrono::Duration::minutes(15);
                    let event_end_time_str =
                        event_end_time_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let expiry_ts = event_end_time_utc.timestamp();

                    let symbol = match id.get(1) {
                        Some(&"btc") => "BTC",
                        _ => todo!(),
                    };

                    let url = format!(
                            "https://polymarket.com/api/crypto/crypto-price?symbol={}&eventStartTime={}&variant=fifteen&endDate={}",
                            symbol,event_start_time_str,event_end_time_str,
                        );

                    println!("url: {:?}", url);
                    let response = reqwest::get(&url).await?;
                    let polly_price: Price = response.json().await?;
                    // println!("polly_price: {:?}", polly_price);
                    let strike_price = polly_price.open_price;
                    println!("strike_price: {}", strike_price);

                    let now_ts = Utc::now().timestamp();
                    let time_remaining_secs = expiry_ts - now_ts;
                    println!("time_remaining_secs: {}", time_remaining_secs);

                    // Skip if expired
                    if time_remaining_secs <= 0 {
                        continue;
                    }

                    let time_to_expiry_years = time_remaining_secs as f64 / (365.0 * 24.0 * 3600.0);

                    // Load orderbook state (Ask Prices)
                    let (yes_ask_cents, no_ask_cents, _, _) = market.poly.load();

                    // Calculate Fair Probability (Target Price)
                    let fair_prob_yes = match self.pricing_model {
                        PricingModel::BlackScholes => self.calculate_prob_bs(
                            btc_price,
                            strike_price,
                            time_to_expiry_years,
                            sigma,
                        ),
                        PricingModel::MonteCarlo => self.calculate_prob_mc(
                            btc_price,
                            strike_price,
                            time_to_expiry_years,
                            sigma,
                        ),
                    };
                    println!("BTC Price: {}", btc_price);
                    println!("Strike Price: {}", strike_price);
                    println!("Time to Expiry: {}", time_to_expiry_years);
                    println!("Sigma: {}", sigma);
                    println!("Fair prob yes: {}", fair_prob_yes);

                    let fair_prob_no = 1.0 - fair_prob_yes;

                    // STRATEGY LOGIC: Probabilistic Edge
                    // If Market Price < Fair Price - Margin, execute.

                    // 1. Check Buy YES opportunity
                    if yes_ask_cents > 0 {
                        let market_price_yes = yes_ask_cents as f64 / 100.0;
                        self.check_and_execute(
                            market,
                            "BUY",
                            market_price_yes,
                            fair_prob_yes,
                            dry_run,
                        )
                        .await?;
                    }

                    // 2. Check Buy NO opportunity
                    if no_ask_cents > 0 {
                        let market_price_no = no_ask_cents as f64 / 100.0;
                        self.check_and_execute(
                            market,
                            "SELL", // Maps to Buy NO Token logic downstream
                            market_price_no,
                            fair_prob_no,
                            dry_run,
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Update rolling 30m price history
    fn update_price_history(&mut self, price: f64) {
        let now = Instant::now();
        self.price_history.push_back((now, price));

        // Trim old data
        while let Some(&(time, _)) = self.price_history.front() {
            if now.duration_since(time).as_secs() > VOLATILITY_WINDOW_SECS {
                self.price_history.pop_front();
            } else {
                break;
            }
        }
    }

    /// Calculate Realized Volatility (Annualized Standard Deviation of Log Returns)
    fn calculate_iv(&self) -> Option<f64> {
        if self.price_history.len() < 10 {
            return None; // Not enough data
        }

        let mut log_returns = Vec::new();
        let mut data_iter = self.price_history.iter();
        let mut prev_price = data_iter.next()?.1;

        for &(_, price) in data_iter {
            let log_ret = (price / prev_price).ln();
            log_returns.push(log_ret);
            prev_price = price;
        }

        if log_returns.is_empty() {
            return None;
        }

        // Calculate standard deviation of log returns
        let n = log_returns.len() as f64;
        let mean = log_returns.iter().sum::<f64>() / n;
        let variance = log_returns.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std_dev = variance.sqrt();

        // Annualize: Sigma = std_dev * sqrt(samples_per_year)
        // Assuming ~1 sample per second from Binance
        let samples_per_year: f64 = 365.0 * 24.0 * 3600.0;
        let annualized_vol = std_dev * samples_per_year.sqrt();

        Some(annualized_vol)
    }

    /// Black-Scholes Binary Call Option Pricing
    /// Price of "Asset > Strike" = N(d2) in Black-Scholes framework for digital options
    fn calculate_prob_bs(&self, s: f64, k: f64, t: f64, sigma: f64) -> f64 {
        if t <= 0.0 {
            return if s > k { 1.0 } else { 0.0 };
        }
        let d2 = ((s / k).ln() + (RISK_FREE_RATE - 0.5 * sigma * sigma) * t) / (sigma * t.sqrt());
        let normal = Normal::new(0.0, 1.0).unwrap();
        normal.cdf(d2)
    }

    /// Monte Carlo Simulation for Binary Call Option
    fn calculate_prob_mc(&self, s: f64, k: f64, t: f64, sigma: f64) -> f64 {
        let iterations = 1000;
        let dt = t; // Single step simulation for simplicity, can be detailed
        let drift = (RISK_FREE_RATE - 0.5 * sigma * sigma) * dt;
        let diffusion = sigma * dt.sqrt();
        let normal = Normal::new(0.0, 1.0).unwrap();
        let mut rng = thread_rng();

        let mut winning_paths = 0;
        for _ in 0..iterations {
            let z = normal.sample(&mut rng);
            let s_t = s * (drift + diffusion * z).exp();
            if s_t > k {
                winning_paths += 1;
            }
        }

        winning_paths as f64 / iterations as f64
    }

    async fn check_and_execute(
        &self,
        market: &AtomicMarketState,
        side: &str, // "BUY" -> Buy Yes Token, "SELL" -> Buy No Token
        current_price: f64,
        target_price: f64,
        dry_run: bool,
    ) -> Result<()> {
        let expected_profit = target_price - current_price;
        let profit_margin = expected_profit - (GAS_FEE_ESTIMATE_CENTS / 100.0);

        if profit_margin > (MIN_SKEW_PROFIT_CENTS / 100.0) {
            let pair = market.pair.as_ref().unwrap();

            // Log the opportunity
            if profit_margin > 0.05 {
                info!(
                    "[0x8dxd] Opportunity! Market: {} | Action: Buy {} | Price: {:.3} | Target: {:.3} | Margin: {:.3} | Model: {:?}",
                    pair.description,
                    if side == "BUY" { "YES" } else { "NO" },
                    current_price,
                    target_price,
                    profit_margin,
                    self.pricing_model
                );
            }

            if !dry_run {
                let token_id = if side == "BUY" {
                    &pair.poly_yes_token
                } else {
                    &pair.poly_no_token
                };

                let size = MAX_POSITION_USD / current_price;

                // Uses buy_fak to buy the specific token (YES or NO token)
                match self.client.buy_fak(token_id, current_price, size).await {
                    Ok(fill) => info!("[0x8dxd] Executed! Matched: {}", fill.filled_size),
                    Err(e) => error!("[0x8dxd] Execution Failed: {}", e),
                }
            }
        }
        Ok(())
    }
}
