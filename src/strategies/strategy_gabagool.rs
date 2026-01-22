use crate::polymarket::Client;
use crate::types::{cents_to_price, GlobalState, MarketPair};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// Gabagool-like pair hedging strategy
pub struct StrategyGabagool {
    state: Arc<RwLock<GlobalState>>,
    client: Client,
    // per-market run totals: market_id -> (qty_yes, cost_yes, qty_no, cost_no)
    totals: HashMap<u16, (f64, f64, f64, f64)>,
    pair_cost_threshold: f64,
    buy_step: f64,
}

impl StrategyGabagool {
    pub async fn new(
        state: Arc<RwLock<GlobalState>>,
        client: Client,
        buy_step: f64,
        pair_cost_threshold: f64,
    ) -> Self {
        info!(
            "[Gabagool] Initialized strategy (buy_step={}, threshold={})",
            buy_step, pair_cost_threshold
        );
        Self {
            state,
            client,
            totals: HashMap::new(),
            pair_cost_threshold,
            buy_step,
        }
    }

    /// Main loop: run whenever Polymarket orderbook updates are received
    pub async fn run(mut self, dry_run: bool) {
        info!("[Gabagool] Running (dry_run={})", dry_run);

        loop {
            // Snapshot necessary market data under read lock
            let mut snapshot: Vec<(u16, Arc<MarketPair>, u16, u16)> = Vec::new();
            {
                let s = self.state.read().await;
                for m in s.markets.iter() {
                    if let Some(pair) = &m.pair {
                        let market_id = m.market_id;
                        let (yes_cents, no_cents, _yes_size, _no_size) = m.poly.load();
                        snapshot.push((market_id, pair.clone(), yes_cents, no_cents));
                    }
                }
            }

            for (market_id, pair, yes_cents, no_cents) in snapshot.into_iter() {
                if yes_cents == 0 || no_cents == 0 {
                    continue; // no price available
                }

                // println!(
                //     "market_id: {}, yes_cents: {}, no_cents: {}",
                //     market_id, yes_cents, no_cents
                // );

                let yes_price = cents_to_price(yes_cents);
                let no_price = cents_to_price(no_cents);

                // Totals for this market
                let entry = self
                    .totals
                    .entry(market_id)
                    .or_insert((0.0f64, 0.0f64, 0.0f64, 0.0f64));
                let (qty_yes, cost_yes, qty_no, cost_no) = *entry;

                // compute current averages (if zero, treat avg as current price)
                let avg_yes = if qty_yes > 0.0 {
                    cost_yes / qty_yes
                } else {
                    yes_price
                };
                let avg_no = if qty_no > 0.0 {
                    cost_no / qty_no
                } else {
                    no_price
                };

                // // Stop condition: locked profit
                // if (qty_yes.min(qty_no)) > (cost_yes + cost_no) {
                //     info!(
                //         "[Gabagool:{}] Profit locked profit:{} cost: {}",
                //         market_id,
                //         qty_yes.min(qty_no),
                //         cost_yes + cost_no
                //     );
                //     continue;
                // }

                // Try YES
                let new_qty_yes = qty_yes + self.buy_step;
                let new_cost_yes = cost_yes + yes_price * self.buy_step;
                let new_avg_yes = new_cost_yes / new_qty_yes;
                let new_pair_cost_yes = new_avg_yes + avg_no;

                // println!(
                //     "[Gabagool:{}] yes_price: {}, no_price: {}, avg_yes: {:.4}, avg_no: {:.4}, new_pair_cost_yes: {:.4}, new_pair_cost_no: {:.4}",
                //     market_id, yes_price, no_price, avg_yes, avg_no, new_pair_cost_yes, new_pair_cost_yes
                // );

                if new_pair_cost_yes < self.pair_cost_threshold {
                    info!(
                        "[Gabagool:{}] Opportunity: BUY YES @ {} -> new_pair_cost={:.4}",
                        market_id, yes_price, new_pair_cost_yes
                    );
                    if !dry_run {
                        let token = pair.poly_yes_token.to_string();
                        let _ = self
                            .client
                            .buy_fak(&token, yes_price, self.buy_step, false)
                            .await;
                    }
                    *entry = (new_qty_yes, new_cost_yes, qty_no, cost_no);
                    // skip NO this round
                    continue;
                }

                // Try NO
                let new_qty_no = qty_no + self.buy_step;
                let new_cost_no = cost_no + no_price * self.buy_step;
                let new_avg_no = new_cost_no / new_qty_no;
                let new_pair_cost_no = avg_yes + new_avg_no;

                if new_pair_cost_no < self.pair_cost_threshold {
                    info!(
                        "[Gabagool:{}] Opportunity: BUY NO @ {} -> new_pair_cost={:.4}",
                        market_id, no_price, new_pair_cost_no
                    );
                    if !dry_run {
                        let token = pair.poly_no_token.to_string();
                        let _ = self
                            .client
                            .buy_fak(&token, no_price, self.buy_step, false)
                            .await;
                    }
                    *entry = (qty_yes, cost_yes, new_qty_no, new_cost_no);
                }

                // init
                if (cost_yes + cost_no) == 0.0 {
                    if yes_price > no_price {
                        // buy no
                        info!(
                            "[Gabagool:{}] Init: BUY NO @ {} -> new_pair_cost={:.4}",
                            market_id, no_price, new_pair_cost_no
                        );
                        if !dry_run {
                            let token = pair.poly_no_token.to_string();
                            let _ = self
                                .client
                                .buy_fak(&token, no_price, self.buy_step, false)
                                .await;
                        }
                        *entry = (qty_yes, cost_yes, new_qty_no, new_cost_no);
                    } else {
                        // buy yes
                        info!(
                            "[Gabagool:{}] Init: BUY YES @ {} -> new_pair_cost={:.4}",
                            market_id, yes_price, new_pair_cost_yes
                        );
                        if !dry_run {
                            let token = pair.poly_yes_token.to_string();
                            let _ = self
                                .client
                                .buy_fak(&token, yes_price, self.buy_step, false)
                                .await;
                        }
                        *entry = (new_qty_yes, new_cost_yes, qty_no, cost_no);
                    }
                }
            }

            // small sleep between markets to avoid spamming
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }
}
