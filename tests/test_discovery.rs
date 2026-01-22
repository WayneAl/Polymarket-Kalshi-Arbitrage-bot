use futures_util::StreamExt;
use prediction_market_arbitrage::polymarket::Client;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info, warn};

#[tokio::test]
async fn test_discover_15m_markets_live() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let dummy_pk = "0000000000000000000000000000000000000000000000000000000000000001";
    let dummy_funder = "0x0000000000000000000000000000000000000000";

    let client = Client::new(dummy_pk, dummy_funder)
        .await
        .expect("Failed to init client");
    println!("Fetching 15m markets from Gamma/Polymarket API...");

    // 2. Execute Discovery
    let markets_res = client.discover_15m_markets().await;

    // 3. Verify Result
    match markets_res {
        Ok(pairs) => {
            println!("✅ Successfully discovered {} markets.", pairs.len());

            if pairs.is_empty() {
                println!(
                    "⚠️  No active 15m markets found. This is possible if none are currently open."
                );
                return;
            }

            for pair in &pairs {
                // Verify basic structure
                assert!(
                    pair.pair_id.starts_with("poly-"),
                    "Pair ID should start with poly-"
                );
                assert!(
                    !pair.poly_yes_token.is_empty(),
                    "YES token should not be empty"
                );
                assert!(
                    !pair.poly_no_token.is_empty(),
                    "NO token should not be empty"
                );

                // Verify parsing logic
                println!("   • {} | Strike: {:?}", pair.pair_id, pair.strike_price);

                // We expect 15m markets to have a strike price if the API logic works
                // (Unless the crypto-price API failed specifically for this one, but usually it works)
                if let Some(strike) = pair.strike_price {
                    assert!(strike > 0.0, "Strike price must be positive");
                } else {
                    println!("     ⚠️  Missing strike price for {}", pair.pair_id);
                    // We don't fail the test here because sometimes price API might return 404/nodata
                }
            }
        }
        Err(e) => {
            panic!("❌ Discovery failed with error: {}", e);
        }
    }
}

#[tokio::test]
async fn test_rtds() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let dummy_pk = "0000000000000000000000000000000000000000000000000000000000000001";
    let dummy_funder = "0x0000000000000000000000000000000000000000";

    let client = Client::new(dummy_pk, dummy_funder)
        .await
        .expect("Failed to init client");

    // Subscribe to all crypto prices from Binance
    info!(
        stream = "crypto_prices",
        "Subscribing to Binance prices (all symbols)"
    );
    match client.rtds.subscribe_crypto_prices(None) {
        Ok(stream) => {
            let mut stream = Box::pin(stream);
            let mut count = 0;

            while let Some(msg) = stream.next().await {
                println!("msg: {:?}", msg);
            }

            while let Ok(Some(result)) = timeout(Duration::from_secs(5), stream.next()).await {
                match result {
                    Ok(price) => {
                        info!(
                            stream = "crypto_prices",
                            symbol = %price.symbol.to_uppercase(),
                            value = %price.value,
                            timestamp = %price.timestamp
                        );
                        count += 1;
                        if count >= 5 {
                            break;
                        }
                    }
                    Err(e) => debug!(stream = "crypto_prices", error = %e),
                }
            }
            info!(stream = "crypto_prices", received = count);
        }
        Err(e) => debug!(stream = "crypto_prices", error = %e),
    }

    // Subscribe to specific crypto symbols
    // in lower case
    let symbols = vec!["btcusdt".to_owned(), "ethusdt".to_owned()];
    info!(
        stream = "crypto_prices_filtered",
        symbols = ?symbols,
        "Subscribing to specific symbols"
    );
    match client.rtds.subscribe_crypto_prices(Some(symbols.clone())) {
        Ok(stream) => {
            let mut stream = Box::pin(stream);
            let mut count = 0;

            while let Ok(Some(result)) = timeout(Duration::from_secs(5), stream.next()).await {
                match result {
                    Ok(price) => {
                        info!(
                            stream = "crypto_prices_filtered",
                            symbol = %price.symbol.to_uppercase(),
                            value = %price.value
                        );
                        count += 1;
                        if count >= 3 {
                            break;
                        }
                    }
                    Err(e) => debug!(stream = "crypto_prices_filtered", error = %e),
                }
            }
            info!(stream = "crypto_prices_filtered", received = count);
        }
        Err(e) => debug!(stream = "crypto_prices_filtered", error = %e),
    }

    // Subscribe to specific Chainlink symbol
    let chainlink_symbol = "btc/usd".to_owned();
    info!(
        stream = "chainlink_prices",
        symbol = %chainlink_symbol,
        "Subscribing to Chainlink price feed"
    );
    match client
        .rtds
        .subscribe_chainlink_prices(Some(chainlink_symbol))
    {
        Ok(stream) => {
            let mut stream = Box::pin(stream);
            let mut count = 0;

            while let Ok(Some(result)) = timeout(Duration::from_secs(5), stream.next()).await {
                match result {
                    Ok(price) => {
                        info!(
                            stream = "chainlink_prices",
                            symbol = %price.symbol,
                            value = %price.value,
                            timestamp = %price.timestamp
                        );
                        count += 1;
                        if count >= 3 {
                            break;
                        }
                    }
                    Err(e) => debug!(stream = "chainlink_prices", error = %e),
                }
            }
            info!(stream = "chainlink_prices", received = count);
        }
        Err(e) => debug!(stream = "chainlink_prices", error = %e),
    }

    // Show subscription count before unsubscribe
    let sub_count = client.rtds.subscription_count();
    info!(
        endpoint = "subscription_count",
        count = sub_count,
        "Before unsubscribe"
    );

    // Demonstrate unsubscribe functionality
    info!("=== Demonstrating unsubscribe ===");

    // Unsubscribe from crypto_prices (Binance)
    info!("Unsubscribing from Binance crypto prices");
    match client.rtds.unsubscribe_crypto_prices() {
        Ok(()) => info!("Successfully unsubscribed from crypto_prices"),
        Err(e) => warn!(error = %e, "Failed to unsubscribe from crypto_prices"),
    }

    // Unsubscribe from chainlink prices
    info!("Unsubscribing from Chainlink prices");
    match client.rtds.unsubscribe_chainlink_prices() {
        Ok(()) => info!("Successfully unsubscribed from chainlink_prices"),
        Err(e) => warn!(error = %e, "Failed to unsubscribe from chainlink_prices"),
    }

    // Show final subscription count after unsubscribe
    let sub_count = client.rtds.subscription_count();
    info!(
        endpoint = "subscription_count",
        count = sub_count,
        "After unsubscribe"
    );

    Ok(())
}
