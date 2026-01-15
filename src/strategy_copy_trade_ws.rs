use anyhow::Result;
use ethers::abi::AbiDecode;
use ethers::prelude::*;
use ethers::providers::{Provider, Ws};
use ethers::types::U256;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::polymarket_clob::SharedAsyncClient;
use crate::types::GlobalState;

// CTF Exchange Contract Address (Polygon)
const CTF_EXCHANGE_ADDR: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

// Event signature for OrderFilled
// event OrderFilled(bytes32 indexed orderHash, address indexed maker, address indexed taker, uint256 makerFillAmount, uint256 takerFillAmount, uint256 fee)
// However, checking previous implementation, we likely used a broad filter.
// The event signature hash for OrderFilled is usually keccak256("OrderFilled(...)")
// Let's use abigen! macro for cleaner code if possible, or just raw logs.
// Previous implementation simulated abigen.
// We will simply use raw log filtering for the topic and target address.

abigen!(
    CtfExchange,
    r#"[
        event OrderFilled(bytes32 indexed orderHash, address indexed maker, address indexed taker, uint256 makerFillAmount, uint256 takerFillAmount, uint256 fee)
    ]"#
);

pub struct StrategyCopyTradeWS {
    state: Arc<RwLock<GlobalState>>,
    _market_id: u16,
    target_address: String,
    client: Arc<SharedAsyncClient>,
    ws_url: String,
}

impl StrategyCopyTradeWS {
    pub async fn new(
        state: Arc<RwLock<GlobalState>>,
        market_id: u16,
        target_address: String,
        client: Arc<SharedAsyncClient>,
        ws_url: String,
    ) -> Self {
        info!(
            "[CopyTradeWS] Initialized strategy for target: {} via WS",
            target_address
        );

        Self {
            state,
            _market_id: market_id,
            target_address,
            client,
            ws_url,
        }
    }

    pub async fn run(self, dry_run: bool) {
        info!(
            "[CopyTradeWS] Connecting to Polygon WS at {}...",
            self.ws_url
        );

        let provider = match Provider::<Ws>::connect(&self.ws_url).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                error!("[CopyTradeWS] Failed to connect to WS: {}", e);
                return;
            }
        };

        let target_addr: Address = self.target_address.parse().expect("Invalid target address");
        let ctf_exchange_addr: Address = CTF_EXCHANGE_ADDR.parse().expect("Invalid CTF address");

        let filter = Filter::new().address(ctf_exchange_addr).topic2(target_addr);

        info!("[CopyTradeWS] Subscribing to OrderFilled events...");

        // Strategy: Subscribe to everything from CTFExchange and filter locally.
        // If volume is too high, this might lag. But for CTFExchange it might be OK?
        // Let's try specific filters if possible.
        // Actually, let's just create ONE stream for ALL OrderFilled events and check addresses.

        let mut stream = match provider.subscribe_logs(&filter).await {
            Ok(s) => s,
            Err(e) => {
                error!("[CopyTradeWS] Failed to subscribe logs: {}", e);
                return;
            }
        };

        info!("[CopyTradeWS] Listening for trades...");

        while let Some(log) = stream.next().await {
            println!("[CopyTradeWS] Log received: {:?}", log);
            // Parse event
            if let Ok(event) = ethers::contract::parse_log::<OrderFilledFilter>(log.clone()) {
                let is_maker = event.maker == target_addr;
                let is_taker = event.taker == target_addr;

                if is_maker || is_taker {
                    let tx_hash = log.transaction_hash.unwrap_or_default();
                    info!(
                        "[CopyTradeWS] 🚨 TRADE DETECTED! Tx: {:?} | Maker: {:?} | Taker: {:?}",
                        tx_hash, event.maker, event.taker
                    );

                    // Speed comparison output helper
                    println!(
                        "SPEED_COMPARE_WS:{} detected at {:?}",
                        tx_hash,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis()
                    );

                    if !dry_run {
                        // Logic to fetch receipt and trade would go here.
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn test_ws() -> anyhow::Result<()> {
    // Instantiate WS Strategy
    dotenvy::dotenv().ok();
    let ws_url = std::env::var("POLYGON_WS_URL").unwrap_or_default();
    let ws = Ws::connect(&ws_url).await?;

    let provider = Provider::new(ws);

    let polymarket_contract: Address =
        Address::from_str("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E")?;

    let filter = Filter::new().address(polymarket_contract);

    let block = provider.get_block_number().await?;
    println!("latest block: {}", block);

    let mut logs = provider.subscribe_logs(&filter).await?;

    while let Some(log) = logs.next().await {
        println!(
            "log block={} tx={:?}",
            log.block_number.unwrap(),
            log.transaction_hash
        );
    }

    Ok(())
}
