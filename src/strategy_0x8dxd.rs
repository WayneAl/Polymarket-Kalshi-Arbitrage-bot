use anyhow::Result;
use regex::Regex;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{error, info};

use crate::binance_ws::BinancePrice;
use crate::polymarket_clob::SharedAsyncClient;
use crate::types::{AtomicMarketState, GlobalState};

// Configurable parameters
const MIN_SKEW_PROFIT_CENTS: f64 = 2.0; // Minimum edge (cents) to execute
const GAS_FEE_ESTIMATE_CENTS: f64 = 1.0; // Estimate per share if not batched
const MAX_POSITION_USD: f64 = 50.0; // Small size per trade as per 0x8dxd style

pub struct Strategy0x8dxd {
    state: Arc<GlobalState>,
    client: Arc<SharedAsyncClient>,
    price_rx: broadcast::Receiver<BinancePrice>,
    regex: Regex,
}

impl Strategy0x8dxd {
    pub fn new(
        state: Arc<GlobalState>,
        client: Arc<SharedAsyncClient>,
        price_rx: broadcast::Receiver<BinancePrice>,
    ) -> Self {
        // Regex to parse "Bitcoin > $95,000"
        let regex = Regex::new(r"Bitcoin\s*>\s*\$?([\d,]+\.?\d*)").expect("Invalid Regex");

        Self {
            state,
            client,
            price_rx,
            regex,
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

    async fn process_tick(&self, binance_price: &BinancePrice, dry_run: bool) -> Result<()> {
        let btc_price = binance_price.mid; // Use mid price

        let market_count = self.state.market_count();
        // Iterate through markets
        for i in 0..market_count {
            if let Some(market) = self.state.get_by_id(i as u16) {
                if let Some(pair) = &market.pair {
                    // Check if it's a Bitcoin market
                    if !pair.description.contains("Bitcoin") {
                        continue;
                    }

                    // Parse Strike Price
                    if let Some(captures) = self.regex.captures(&pair.description) {
                        if let Some(strike_match) = captures.get(1) {
                            let strike_str = strike_match.as_str().replace(",", "");
                            if let Ok(strike_price) = strike_str.parse::<f64>() {
                                // Load orderbook state (Ask Prices)
                                let (yes_ask_cents, no_ask_cents, _, _) = market.poly.load();

                                // STRATEGY LOGIC
                                // If BTC > Strike, we want to hold YES.
                                // If YES is cheap (Ask < Target), Buy YES.

                                if btc_price > strike_price * 1.001 {
                                    // Target: YES = 1.0 (Winning)
                                    if yes_ask_cents > 0 && yes_ask_cents < 100 {
                                        let p_yes = yes_ask_cents as f64 / 100.0;
                                        self.check_and_execute(market, "BUY", p_yes, 0.99, dry_run)
                                            .await?;
                                    }
                                }
                                // If BTC < Strike, we want to hold NO.
                                // If NO is cheap (Ask < Target), Buy NO.
                                else if btc_price < strike_price * 0.999 {
                                    // Target: NO = 1.0 (Winning)
                                    if no_ask_cents > 0 && no_ask_cents < 100 {
                                        let p_no = no_ask_cents as f64 / 100.0;
                                        self.check_and_execute(market, "SELL", p_no, 0.99, dry_run)
                                            .await?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
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
                    "[0x8dxd] Opportunity! Market: {} | Action: Buy {} | Price: {:.3} | Target: {:.3} | Margin: {:.3}",
                    pair.description,
                    if side == "BUY" { "YES" } else { "NO" },
                    current_price,
                    target_price,
                    profit_margin
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
