use anyhow::Result;
use chrono::Utc;
use rand::distributions::Distribution;
use rand::thread_rng;

use serde_json::Value;
use statrs::distribution::{ContinuousCDF, Normal};
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::polymarket::Client;
use crate::types::GlobalState;
use polymarket_client_sdk::rtds::{ChainlinkPrice, CryptoPrice};
use rust_decimal::prelude::ToPrimitive;

// Configurable parameters
const MIN_SKEW_PROFIT_CENTS: f64 = 2.0; // Minimum edge (cents) to execute
const GAS_FEE_ESTIMATE_CENTS: f64 = 1.0; // Estimate per share if not batched
const MAX_POSITION_USD: f64 = 50.0; // Small size per trade as per 0x8dxd style

const RISK_FREE_RATE: f64 = 0.04; // 4% annual risk-free rate

#[derive(Clone, Copy, Debug)]
pub enum PricingModel {
    BlackScholes,
    MonteCarlo,
}

pub struct Strategy0x8dxd {
    state: Arc<tokio::sync::RwLock<GlobalState>>,
    market_id: u16,
    asset: String, // e.g., "BTC", "ETH"
    client: Client,
    price_history: VecDeque<(i64, f64)>,

    pricing_model: PricingModel,

    strike_price: f64,
    expiry_ts: i64,
    binance_ref_price: f64,

    default_sigma: f64,

    // Store latest chainlink price (updated in background)
    latest_chainlink_price: Arc<tokio::sync::Mutex<Option<ChainlinkPrice>>>,
}

impl Strategy0x8dxd {
    pub async fn new(
        state: Arc<tokio::sync::RwLock<GlobalState>>,
        market_id: u16,
        asset: String,
        default_sigma: f64,
        client: Client,
    ) -> Self {
        // Select model from environment or default to Black-Scholes
        let pricing_model = match std::env::var("PRICING_MODEL").unwrap_or_default().as_str() {
            "monte_carlo" | "mc" => PricingModel::MonteCarlo,
            _ => PricingModel::BlackScholes,
        };

        // Fetch market info (strike, expiry, ref price)
        let (strike_price, expiry_ts, _binance_ref_price) = {
            let s = state.read().await;
            let market = s.get_by_id(market_id).expect("Invalid market_id");
            let p = market.pair.as_ref().unwrap();

            let parts: Vec<&str> = p.pair_id.split('-').collect();
            let start_time = parts.last().unwrap().parse::<i64>().unwrap_or(0);
            let expiry_ts = start_time + 900;
            let strike_price = p.strike_price.unwrap_or(0.0);

            // Temporarily release lock to await async call? No, get_binance_price_at is async.
            // We need to drop the lock before awaiting.
            (strike_price, expiry_ts, start_time)
        };
        // Re-acquire lock logic avoided by extracting values.

        // Get Binance ref price
        let binance_ref_price = if strike_price > 0.0 {
            match Self::get_binance_price_at(&asset, expiry_ts - 900).await {
                // start_time is expiry - 900
                Ok(p) => p,
                Err(e) => {
                    warn!("[0x8dxd] Failed to fetch binance historical price: {}", e);
                    strike_price // Fallback
                }
            }
        } else {
            0.0
        };

        {
            let s = state.read().await;
            let market = s.get_by_id(market_id).expect("Invalid market_id");
            info!(
                "[0x8dxd] Initialized strategy for {} (Asset: {}) with Pricing Model: {:?}. Strike={}, Ref={}, Sigma={}",
                market.pair.as_ref().map(|p| p.pair_id.as_ref()).unwrap_or("Unknown"),
                asset,
                pricing_model,
                strike_price,
                binance_ref_price,
                default_sigma
            );
        }

        Self {
            state,
            market_id,
            asset,

            client,
            price_history: VecDeque::new(),

            pricing_model,
            strike_price,
            expiry_ts,
            binance_ref_price,
            default_sigma,
            latest_chainlink_price: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub async fn run(mut self, dry_run: bool) {
        let pair_id = {
            let s = self.state.read().await;
            let m = s.get_by_id(self.market_id).unwrap();
            m.pair
                .as_ref()
                .map(|p| p.pair_id.to_string())
                .unwrap_or_default()
        };

        info!("[0x8dxd] Strategy loop started for {}", pair_id);

        // Subscribe to both feeds
        let mut crypto_rx = self.client.subscribe_prices();
        let mut chainlink_rx = self.client.subscribe_chainlink_prices();
        let asset_lower = self.asset.to_lowercase();
        let latest_chainlink = self.latest_chainlink_price.clone();

        // Background task: continuously update latest chainlink price
        tokio::spawn({
            let latest_chainlink = latest_chainlink.clone();
            let asset_lower = asset_lower.clone();
            async move {
                while let Ok(price) = chainlink_rx.recv().await {
                    // Filter for this strategy's asset
                    if !price.symbol.to_lowercase().starts_with(&asset_lower) {
                        continue;
                    }
                    *latest_chainlink.lock().await = Some(price);
                }
            }
        });

        // Main task: process crypto prices with access to latest chainlink
        while let Ok(price) = crypto_rx.recv().await {
            // Filter for this strategy's asset
            if !price.symbol.to_lowercase().starts_with(&asset_lower) {
                continue;
            }

            // Get latest chainlink price if available
            let chainlink_price = latest_chainlink.lock().await.clone();
            if chainlink_price.is_none() {
                continue;
            }
            let chainlink_price = chainlink_price.unwrap();

            info!(
                "[0x8dxd] Crypto: {} ({}), Chainlink: {} ({}), Delta: {}",
                price.value,
                &price.symbol,
                chainlink_price.value,
                &chainlink_price.symbol,
                (price.value - chainlink_price.value)
            );

            let price = chainlink_price.value.to_f64().unwrap_or(0.0);

            match self.process_tick(price, dry_run).await {
                Ok(should_stop) => {
                    if should_stop {
                        info!("[0x8dxd] Strategy expired for {}", pair_id);
                        break;
                    }
                }
                Err(e) => error!("[0x8dxd] Error processing tick for {}: {}", pair_id, e),
            }
        }
        info!("[0x8dxd] Strategy Stopped for {}", pair_id);
    }

    /// Returns Ok(true) if the strategy should stop (expired), Ok(false) otherwise.
    async fn process_tick(&mut self, price: f64, dry_run: bool) -> Result<bool> {
        if price == 0.0 {
            return Ok(false);
        }

        // Update history and calc volatility
        let now_exec = Utc::now().timestamp();
        self.update_history(now_exec, price);

        // Calculate Realized Volatility (Annualized)
        let mut sigma = self.get_iv().unwrap_or(self.default_sigma);
        sigma = sigma.max(self.default_sigma);

        // check timestamp from price? rtds price has timestamp usually

        let now_ts = Utc::now().timestamp();
        let time_remaining_secs = self.expiry_ts - now_ts;

        if time_remaining_secs <= 0 {
            info!("[0x8dxd] Market expired, settle at {}", price);
            return Ok(true);
        }

        let time_to_expiry_years = time_remaining_secs as f64 / (365.0 * 24.0 * 3600.0);

        // let price = if self.binance_ref_price > 0.0 {
        //     let offset = self.binance_ref_price - self.strike_price;
        //     price - offset
        // } else {
        //     price
        // };

        if self.strike_price == 0.0 {
            return Ok(false);
        }

        let fair_prob_yes = match self.pricing_model {
            PricingModel::BlackScholes => {
                Self::calculate_prob_bs(price, self.strike_price, time_to_expiry_years, sigma)
            }
            PricingModel::MonteCarlo => {
                Self::calculate_prob_mc(price, self.strike_price, time_to_expiry_years, sigma)
            }
        };

        info!(
            target: "strategy_0x8dxd",
            "Price: {}, Strike: {}, Time: {}, Prob: {}s, Sigma: {}",
            price, self.strike_price, time_remaining_secs, fair_prob_yes, sigma
        );

        let fair_prob_no = 1.0 - fair_prob_yes;

        let (poly_yes_token, poly_no_token, yes_ask, no_ask) = {
            let s = self.state.read().await;
            let m = s.get_by_id(self.market_id).unwrap();
            let p = m.pair.as_ref().unwrap();
            let (ya, na, _, _) = m.poly.load();
            (p.poly_yes_token.clone(), p.poly_no_token.clone(), ya, na)
        };

        info!("YES ASK: {} NO ASK: {}", yes_ask, no_ask);

        if yes_ask + no_ask < 100 {
            info!("YES ASK: {} NO ASK: {}", yes_ask, no_ask);
        }

        // let mut opps = Vec::new();
        // if yes_ask > 0 {
        //     let market_price_yes = yes_ask as f64 / 100.0;
        //     if let Some(msg) = self
        //         .check_and_execute(
        //             "BUY",
        //             market_price_yes,
        //             fair_prob_yes,
        //             dry_run,
        //             &poly_yes_token,
        //             &poly_no_token,
        //         )
        //         .await?
        //     {
        //         opps.push(msg);
        //     }
        // }

        // if no_ask > 0 {
        //     let market_price_no = no_ask as f64 / 100.0;
        //     if let Some(msg) = self
        //         .check_and_execute(
        //             "SELL",
        //             market_price_no,
        //             fair_prob_no,
        //             dry_run,
        //             &poly_yes_token,
        //             &poly_no_token,
        //         )
        //         .await?
        //     {
        //         opps.push(msg);
        //     }
        // }

        // if !opps.is_empty() {
        //     // Access pair_id for logging
        //     let pair_id = {
        //         let s = self.state.read().await;
        //         let m = s.get_by_id(self.market_id).unwrap();
        //         m.pair
        //             .as_ref()
        //             .map(|p| p.pair_id.to_string())
        //             .unwrap_or_default()
        //     };
        //     let action_status = opps.join(" | ");
        //     info!("[{}] {}", pair_id, action_status);
        // }

        Ok(false)
    }

    fn calculate_prob_bs(s: f64, k: f64, t: f64, sigma: f64) -> f64 {
        if t <= 0.0 {
            return if s > k { 1.0 } else { 0.0 };
        }
        let d2 = ((s / k).ln() + (RISK_FREE_RATE - 0.5 * sigma * sigma) * t) / (sigma * t.sqrt());
        let normal = Normal::new(0.0, 1.0).unwrap();
        normal.cdf(d2)
    }

    fn calculate_prob_mc(s: f64, k: f64, t: f64, sigma: f64) -> f64 {
        let iterations = 10000;
        let dt = t;
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
        side: &str,
        current_price: f64,
        target_price: f64,
        dry_run: bool,
        yes_token: &Arc<str>,
        no_token: &Arc<str>,
    ) -> Result<Option<String>> {
        let expected_profit = target_price - current_price;
        let profit_margin = expected_profit - (GAS_FEE_ESTIMATE_CENTS / 100.0);

        if profit_margin > (MIN_SKEW_PROFIT_CENTS / 100.0) {
            let opp_msg = format!(
                "Buy {} @ {:.2} (M:{:.2})",
                if side == "BUY" { "YES" } else { "NO" },
                current_price,
                profit_margin
            );

            if !dry_run {
                let token_id = if side == "BUY" { yes_token } else { no_token };

                let size = MAX_POSITION_USD / current_price;

                match self
                    .client
                    .buy_fak(token_id, current_price, size, true)
                    .await
                {
                    Ok(fill) => info!("[0x8dxd] Executed! Matched: {}", fill.filled_size),
                    Err(e) => error!("[0x8dxd] Execution Failed: {}", e),
                }
            }

            return Ok(Some(opp_msg));
        }
        Ok(None)
    }

    async fn get_binance_price_at(asset: &str, start_time_ms: i64) -> Result<f64> {
        let symbol = format!("{}USDT", asset.to_uppercase());

        // Note: startTime parameter for Binance API is milliseconds.
        // Make sure start_time_ms is actually ms. In constructor we passed (expiry - 900) which is seconds.
        // So we need to multiply by 1000.
        let start_ts_param = start_time_ms * 1000;

        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol={}&interval=1m&startTime={}&limit=1",
            symbol, start_ts_param
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(4))
            .build()?;
        let response = client.get(&url).send().await?;
        let json: Value = response.json().await?;

        if let Some(klines) = json.as_array() {
            if let Some(first_kline) = klines.first() {
                if let Some(first_kline_arr) = first_kline.as_array() {
                    if let Some(open_str) = first_kline_arr.get(1).and_then(|v| v.as_str()) {
                        let price = open_str.parse::<f64>()?;
                        return Ok(price);
                    }
                }
            }
        }

        anyhow::bail!("Failed to parse Binance historical price")
    }

    fn update_history(&mut self, timestamp: i64, price: f64) {
        // Limit updates to once per second
        if let Some(&(last_time, _)) = self.price_history.back() {
            if timestamp - last_time < 1 {
                return;
            }
        }

        self.price_history.push_back((timestamp, price));

        // Prune old (30 mins = 1800s)
        while let Some(&(time, _)) = self.price_history.front() {
            if timestamp - time > 1800 {
                self.price_history.pop_front();
            } else {
                break;
            }
        }
    }

    fn get_iv(&self) -> Option<f64> {
        if self.price_history.len() < 10 {
            return None;
        }

        // Calculate IV
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

        let n = log_returns.len() as f64;
        let mean = log_returns.iter().sum::<f64>() / n;
        let variance = log_returns.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std_dev = variance.sqrt();

        let samples_per_year: f64 = 365.0 * 24.0 * 3600.0;
        let annualized_vol = std_dev * samples_per_year.sqrt();
        Some(annualized_vol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_prob_bs() {
        let prob_itm = Strategy0x8dxd::calculate_prob_bs(150.0, 100.0, 0.1, 0.5);
        assert!(prob_itm > 0.9);

        let prob_otm = Strategy0x8dxd::calculate_prob_bs(50.0, 100.0, 0.1, 0.5);
        assert!(prob_otm < 0.1);

        let prob_expired_itm = Strategy0x8dxd::calculate_prob_bs(150.0, 100.0, 0.0, 0.5);
        assert_eq!(prob_expired_itm, 1.0);

        let prob_expired_otm = Strategy0x8dxd::calculate_prob_bs(50.0, 100.0, 0.0, 0.5);
        assert_eq!(prob_expired_otm, 0.0);
    }
}
