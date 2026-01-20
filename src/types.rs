//! Core type definitions and data structures for the arbitrage trading system.
//!
//! This module provides the foundational types for market state management,
//! orderbook representation, and arbitrage opportunity detection.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

// === Market Types ===

/// Market category for a matched trading pair
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarketType {
    /// Moneyline/outright winner market
    Moneyline,
    /// Point spread market
    Spread,
    /// Total/over-under market
    Total,
    /// Both teams to score market
    Btts,
}

impl std::fmt::Display for MarketType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MarketType::Moneyline => write!(f, "moneyline"),
            MarketType::Spread => write!(f, "spread"),
            MarketType::Total => write!(f, "total"),
            MarketType::Btts => write!(f, "btts"),
        }
    }
}

/// A matched trading pair between Kalshi and Polymarket platforms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketPair {
    /// Unique identifier for this market pair
    pub pair_id: Arc<str>,
    /// Type of market (moneyline, spread, total, etc.)
    pub market_type: MarketType,
    /// Human-readable market description
    pub description: Arc<str>,
    /// Polymarket market slug
    pub poly_slug: Arc<str>,
    /// Polymarket YES outcome token address
    pub poly_yes_token: Arc<str>,
    /// Polymarket NO outcome token address
    pub poly_no_token: Arc<str>,
    /// The strike/open price of the market (e.g. price at start of 15m window)
    pub strike_price: Option<f64>,

    pub expiry_timestamp: Option<i64>,
}

/// Price representation in cents (1-99 for $0.01-$0.99), 0 indicates no price available
pub type PriceCents = u16;

/// Size representation in cents (dollar amount × 100), maximum ~$655k per side
pub type SizeCents = u16;

/// Maximum number of concurrently tracked markets
pub const MAX_MARKETS: usize = 1024;

/// Sentinel value indicating no price is currently available
pub const NO_PRICE: PriceCents = 0;

/// Pack orderbook state into a single u64 for atomic operations.
/// Bit layout: [yes_ask:16][no_ask:16][yes_size:16][no_size:16]
#[inline(always)]
pub fn pack_orderbook(
    yes_ask: PriceCents,
    no_ask: PriceCents,
    yes_size: SizeCents,
    no_size: SizeCents,
) -> u64 {
    ((yes_ask as u64) << 48)
        | ((no_ask as u64) << 32)
        | ((yes_size as u64) << 16)
        | (no_size as u64)
}

/// Unpack a u64 orderbook representation back into its component values
#[inline(always)]
pub fn unpack_orderbook(packed: u64) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
    let yes_ask = ((packed >> 48) & 0xFFFF) as PriceCents;
    let no_ask = ((packed >> 32) & 0xFFFF) as PriceCents;
    let yes_size = ((packed >> 16) & 0xFFFF) as SizeCents;
    let no_size = (packed & 0xFFFF) as SizeCents;
    (yes_ask, no_ask, yes_size, no_size)
}

/// Lock-free orderbook state for a single trading platform.
/// Uses atomic operations for thread-safe, zero-copy price updates.
#[repr(align(64))]
pub struct AtomicOrderbook {
    /// Packed orderbook state: [yes_ask:16][no_ask:16][yes_size:16][no_size:16]
    packed: AtomicU64,
}

impl AtomicOrderbook {
    pub const fn new() -> Self {
        Self {
            packed: AtomicU64::new(0),
        }
    }

    /// Load current state
    #[inline(always)]
    pub fn load(&self) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
        unpack_orderbook(self.packed.load(Ordering::Acquire))
    }

    /// Store new state
    #[inline(always)]
    pub fn store(
        &self,
        yes_ask: PriceCents,
        no_ask: PriceCents,
        yes_size: SizeCents,
        no_size: SizeCents,
    ) {
        self.packed.store(
            pack_orderbook(yes_ask, no_ask, yes_size, no_size),
            Ordering::Release,
        );
    }

    /// Update YES side only
    #[inline(always)]
    pub fn update_yes(&self, yes_ask: PriceCents, yes_size: SizeCents) {
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (_, no_ask, _, no_size) = unpack_orderbook(current);
            let new = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
            match self.packed.compare_exchange_weak(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }

    /// Update NO side only
    #[inline(always)]
    pub fn update_no(&self, no_ask: PriceCents, no_size: SizeCents) {
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (yes_ask, _, yes_size, _) = unpack_orderbook(current);
            let new = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
            match self.packed.compare_exchange_weak(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }
}

impl Default for AtomicOrderbook {
    fn default() -> Self {
        Self::new()
    }
}

/// Complete market state tracking orderbooks for a single market
pub struct AtomicMarketState {
    /// Polymarket platform orderbook state
    pub poly: AtomicOrderbook,
    /// Market pair metadata (immutable after discovery phase)
    pub pair: Option<Arc<MarketPair>>,
    /// Unique market identifier for O(1) lookups
    pub market_id: u16,
}

impl AtomicMarketState {
    pub fn new(market_id: u16) -> Self {
        Self {
            poly: AtomicOrderbook::new(),
            pair: None,
            market_id,
        }
    }
}

/// Convert f64 price (0.01-0.99) to PriceCents (1-99)
#[inline(always)]
pub fn price_to_cents(price: f64) -> PriceCents {
    ((price * 100.0).round() as PriceCents).clamp(0, 99)
}

/// Convert PriceCents back to f64
#[inline(always)]
pub fn cents_to_price(cents: PriceCents) -> f64 {
    cents as f64 / 100.0
}

/// Parse price from string "0.XX" format (Polymarket)
/// Returns 0 if parsing fails
#[inline(always)]
pub fn parse_price(s: &str) -> PriceCents {
    let bytes = s.as_bytes();
    // Handle "0.XX" format (4 chars)
    if bytes.len() == 4 && bytes[0] == b'0' && bytes[1] == b'.' {
        let d1 = bytes[2].wrapping_sub(b'0');
        let d2 = bytes[3].wrapping_sub(b'0');
        if d1 < 10 && d2 < 10 {
            return (d1 as u16 * 10 + d2 as u16) as PriceCents;
        }
    }
    // Handle "0.X" format (3 chars) for prices like 0.5
    if bytes.len() == 3 && bytes[0] == b'0' && bytes[1] == b'.' {
        let d = bytes[2].wrapping_sub(b'0');
        if d < 10 {
            return (d as u16 * 10) as PriceCents;
        }
    }
    // Fallback to standard parse
    s.parse::<f64>().map(|p| price_to_cents(p)).unwrap_or(0)
}

/// Global market state manager for all tracked markets across both platforms
pub struct GlobalState {
    /// Market states indexed by market_id for O(1) access
    pub markets: Vec<AtomicMarketState>,

    /// Next available market identifier (monotonically increasing)
    next_market_id: u16,

    /// O(1) lookup map: pre-hashed Polymarket YES token → market_id
    pub poly_yes_to_id: FxHashMap<u64, u16>,

    /// O(1) lookup map: pre-hashed Polymarket NO token → market_id
    pub poly_no_to_id: FxHashMap<u64, u16>,
}

impl GlobalState {
    pub fn new() -> Self {
        // Allocate market slots
        let markets: Vec<AtomicMarketState> = (0..MAX_MARKETS)
            .map(|i| AtomicMarketState::new(i as u16))
            .collect();

        Self {
            markets,
            next_market_id: 0,
            poly_yes_to_id: FxHashMap::default(),
            poly_no_to_id: FxHashMap::default(),
        }
    }

    /// Add a market pair, returns market_id
    pub fn add_pair(&mut self, pair: MarketPair) -> Option<u16> {
        if self.next_market_id as usize >= MAX_MARKETS {
            return None;
        }

        let market_id = self.next_market_id;
        self.next_market_id += 1;

        // Pre-compute hashes
        let poly_yes_hash = fxhash_str(&pair.poly_yes_token);
        let poly_no_hash = fxhash_str(&pair.poly_no_token);

        // Update lookup maps
        self.poly_yes_to_id.insert(poly_yes_hash, market_id);
        self.poly_no_to_id.insert(poly_no_hash, market_id);

        // Store pair
        self.markets[market_id as usize].pair = Some(Arc::new(pair));

        Some(market_id)
    }

    /// Get market by Poly YES token hash (O(1))
    #[inline(always)]
    #[allow(dead_code)]
    pub fn get_by_poly_yes_hash(&self, hash: u64) -> Option<&AtomicMarketState> {
        let id = *self.poly_yes_to_id.get(&hash)?;
        Some(&self.markets[id as usize])
    }

    /// Get market by Poly NO token hash (O(1))
    #[inline(always)]
    #[allow(dead_code)]
    pub fn get_by_poly_no_hash(&self, hash: u64) -> Option<&AtomicMarketState> {
        let id = *self.poly_no_to_id.get(&hash)?;
        Some(&self.markets[id as usize])
    }

    /// Get market_id by Poly YES token hash
    #[inline(always)]
    #[allow(dead_code)]
    pub fn id_by_poly_yes_hash(&self, hash: u64) -> Option<u16> {
        self.poly_yes_to_id.get(&hash).copied()
    }

    /// Get market_id by Poly NO token hash
    #[inline(always)]
    #[allow(dead_code)]
    pub fn id_by_poly_no_hash(&self, hash: u64) -> Option<u16> {
        self.poly_no_to_id.get(&hash).copied()
    }

    /// Get market by ID
    #[inline(always)]
    pub fn get_by_id(&self, id: u16) -> Option<&AtomicMarketState> {
        if (id as usize) < self.markets.len() {
            Some(&self.markets[id as usize])
        } else {
            None
        }
    }

    pub fn market_count(&self) -> usize {
        self.next_market_id as usize
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Fast string hashing function using FxHash for O(1) lookups
#[inline(always)]
pub fn fxhash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

// === Platform Enum ===

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    // =========================================================================
    // Pack/Unpack Tests - Verify bit manipulation correctness
    // =========================================================================

    #[test]
    fn test_pack_unpack_roundtrip() {
        // Test various values pack and unpack correctly
        let test_cases = vec![
            (50, 50, 1000, 1000),       // Common mid-price
            (1, 99, 100, 100),          // Edge prices
            (99, 1, 65535, 65535),      // Max sizes
            (0, 0, 0, 0),               // All zeros
            (NO_PRICE, NO_PRICE, 0, 0), // No prices
        ];

        for (yes_ask, no_ask, yes_size, no_size) in test_cases {
            let packed = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
            let (y, n, ys, ns) = unpack_orderbook(packed);
            assert_eq!(
                (y, n, ys, ns),
                (yes_ask, no_ask, yes_size, no_size),
                "Roundtrip failed for ({}, {}, {}, {})",
                yes_ask,
                no_ask,
                yes_size,
                no_size
            );
        }
    }

    #[test]
    fn test_pack_bit_layout() {
        // Verify the exact bit layout: [yes_ask:16][no_ask:16][yes_size:16][no_size:16]
        let packed = pack_orderbook(0xABCD, 0x1234, 0x5678, 0x9ABC);

        assert_eq!(
            (packed >> 48) & 0xFFFF,
            0xABCD,
            "yes_ask should be in bits 48-63"
        );
        assert_eq!(
            (packed >> 32) & 0xFFFF,
            0x1234,
            "no_ask should be in bits 32-47"
        );
        assert_eq!(
            (packed >> 16) & 0xFFFF,
            0x5678,
            "yes_size should be in bits 16-31"
        );
        assert_eq!(packed & 0xFFFF, 0x9ABC, "no_size should be in bits 0-15");
    }

    // =========================================================================
    // AtomicOrderbook Tests
    // =========================================================================

    #[test]
    fn test_atomic_orderbook_store_load() {
        let book = AtomicOrderbook::new();

        // Initially all zeros
        let (y, n, ys, ns) = book.load();
        assert_eq!((y, n, ys, ns), (0, 0, 0, 0));

        // Store and load
        book.store(45, 55, 500, 600);
        let (y, n, ys, ns) = book.load();
        assert_eq!((y, n, ys, ns), (45, 55, 500, 600));
    }

    #[test]
    fn test_atomic_orderbook_update_yes() {
        let book = AtomicOrderbook::new();

        // Set initial state
        book.store(40, 60, 100, 200);

        // Update only YES side
        book.update_yes(42, 150);

        let (y, n, ys, ns) = book.load();
        assert_eq!(y, 42, "YES ask should be updated");
        assert_eq!(ys, 150, "YES size should be updated");
        assert_eq!(n, 60, "NO ask should be unchanged");
        assert_eq!(ns, 200, "NO size should be unchanged");
    }

    #[test]
    fn test_atomic_orderbook_update_no() {
        let book = AtomicOrderbook::new();

        // Set initial state
        book.store(40, 60, 100, 200);

        // Update only NO side
        book.update_no(58, 250);

        let (y, n, ys, ns) = book.load();
        assert_eq!(y, 40, "YES ask should be unchanged");
        assert_eq!(ys, 100, "YES size should be unchanged");
        assert_eq!(n, 58, "NO ask should be updated");
        assert_eq!(ns, 250, "NO size should be updated");
    }

    #[test]
    fn test_atomic_orderbook_concurrent_updates() {
        // Verify correctness under concurrent access
        let book = Arc::new(AtomicOrderbook::new());
        book.store(50, 50, 1000, 1000);

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let book = book.clone();
                thread::spawn(move || {
                    for _ in 0..1000 {
                        if i % 2 == 0 {
                            book.update_yes(45 + (i as u16), 500);
                        } else {
                            book.update_no(55 + (i as u16), 500);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // State should be consistent (not corrupted)
        let (y, n, ys, ns) = book.load();
        assert!(y > 0 && y < 100, "YES ask should be valid");
        assert!(n > 0 && n < 100, "NO ask should be valid");
        assert_eq!(ys, 500, "YES size should be consistent");
        assert_eq!(ns, 500, "NO size should be consistent");
    }

    // =========================================================================
    // GlobalState Tests
    // =========================================================================

    fn make_test_pair(id: &str) -> MarketPair {
        MarketPair {
            pair_id: id.into(),
            market_type: MarketType::Moneyline,
            description: format!("Test Market {}", id).into(),
            poly_slug: format!("test-{}", id).into(),
            poly_yes_token: format!("yes_token_{}", id).into(),
            poly_no_token: format!("no_token_{}", id).into(),
            strike_price: None,
            expiry_timestamp: None,
        }
    }

    #[test]
    fn test_global_state_add_pair() {
        let mut state = GlobalState::new();

        let pair = make_test_pair("001");
        let poly_yes = pair.poly_yes_token.clone();
        let poly_no = pair.poly_no_token.clone();

        let id = state.add_pair(pair).expect("Should add pair");

        assert_eq!(id, 0, "First market should have id 0");
        assert_eq!(state.market_count(), 1);

        // Verify lookups work
        let poly_yes_hash = fxhash_str(&poly_yes);
        let poly_no_hash = fxhash_str(&poly_no);

        assert!(state.poly_yes_to_id.contains_key(&poly_yes_hash));
        assert!(state.poly_no_to_id.contains_key(&poly_no_hash));
    }

    #[test]
    fn test_global_state_lookups() {
        let mut state = GlobalState::new();

        let pair = make_test_pair("002");
        let poly_yes = pair.poly_yes_token.clone();

        let id = state.add_pair(pair).unwrap();

        // Test get_by_id
        let market = state.get_by_id(id).expect("Should find by id");
        assert!(market.pair.is_some());

        // Test get_by_poly_yes_hash
        let market = state
            .get_by_poly_yes_hash(fxhash_str(&poly_yes))
            .expect("Should find by Poly YES hash");
        assert!(market.pair.is_some());

        // Test id lookups
        assert_eq!(state.id_by_poly_yes_hash(fxhash_str(&poly_yes)), Some(id));
    }

    #[test]
    fn test_global_state_multiple_markets() {
        let mut state = GlobalState::new();

        // Add multiple markets
        for i in 0..10 {
            let pair = make_test_pair(&format!("{:03}", i));
            let id = state.add_pair(pair).unwrap();
            assert_eq!(id, i as u16);
        }

        assert_eq!(state.market_count(), 10);

        // All should be findable
        for i in 0..10 {
            let market = state.get_by_id(i as u16);
            assert!(market.is_some(), "Market {} should exist", i);
        }
    }

    #[test]
    fn test_global_state_update_prices() {
        let mut state = GlobalState::new();

        let pair = make_test_pair("003");
        let id = state.add_pair(pair).unwrap();

        // Update Poly prices
        let market = state.get_by_id(id).unwrap();
        market.poly.store(44, 56, 700, 800);

        // Verify prices
        let (p_yes, p_no, p_yes_sz, p_no_sz) = market.poly.load();
        assert_eq!((p_yes, p_no, p_yes_sz, p_no_sz), (44, 56, 700, 800));
    }

    // =========================================================================
    // fxhash_str Tests
    // =========================================================================

    #[test]
    fn test_fxhash_str_consistency() {
        let s = "KXEPLGAME-25DEC27CFCARS-CFC";

        // Same string should always produce same hash
        let h1 = fxhash_str(s);
        let h2 = fxhash_str(s);
        assert_eq!(h1, h2);

        // Different strings should (almost certainly) produce different hashes
        let h3 = fxhash_str("KXEPLGAME-25DEC27CFCARS-ARS");
        assert_ne!(h1, h3);
    }
}
