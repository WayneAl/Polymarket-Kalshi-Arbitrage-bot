use prediction_market_arbitrage::polymarket::Client;

#[tokio::test]
async fn test_discover_15m_markets_live() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    // 1. Initialize Client
    // We need valid-looking fake credentials because we don't want to fail on parsing
    // but the functionality being tested (Gamma) doesn't typically require auth if implemented right.
    // However, our Client::new implementation parses the PK.
    // Use a random hex string for PK.
    let dummy_pk = "0000000000000000000000000000000000000000000000000000000000000001";
    let dummy_funder = "0x0000000000000000000000000000000000000000";
    let host = "https://clob.polymarket.com";

    let client = Client::new(host, 137, dummy_pk, dummy_funder)
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
