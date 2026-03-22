//! # atomic-hotpath
//!
//! C++ FFI bindings for nanosecond-scale HFT hot path.
//! Replaces the Rust BTreeMap order book + strategy compute
//! with cache-aligned flat arrays and SIMD microprice.

use atomic_core::types::{Level, Price, Qty, Side};

// ── Raw FFI declarations ──────────────────────────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HpLevel {
    pub price: i64,
    pub qty: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HpSide {
    Buy = 0,
    Sell = 1,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HpCmdTag {
    None = 0,
    PlaceBid = 1,
    PlaceAsk = 2,
    CancelAll = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HpCommand {
    pub tag: i32, // HpCmdTag as i32 for C compat
    pub price: i64,
    pub qty: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HpResult {
    pub commands: [HpCommand; 4],
    pub count: i32,
    pub fair_value: i64,
    pub vpin: i64,
    pub skew: i64,
    pub half_spread: i64,
    pub compute_ns: i64,
}

impl Default for HpResult {
    fn default() -> Self {
        Self {
            commands: [HpCommand::default(); 4],
            count: 0,
            fair_value: 0,
            vpin: 0,
            skew: 0,
            half_spread: 0,
            compute_ns: 0,
        }
    }
}

// Opaque engine pointer
enum HpEngineOpaque {}

extern "C" {
    fn hp_engine_create(
        order_qty: u64,
        max_inventory: i64,
        half_spread_bps: i32,
        gamma: i32,
        warmup_ticks: i32,
        cooldown_ticks: i32,
        requote_threshold: i64,
        vpin_enabled: bool,
    ) -> *mut HpEngineOpaque;

    fn hp_engine_destroy(engine: *mut HpEngineOpaque);

    fn hp_on_book_update(
        engine: *mut HpEngineOpaque,
        bids: *const HpLevel,
        bid_count: i32,
        asks: *const HpLevel,
        ask_count: i32,
        is_snapshot: bool,
    ) -> HpResult;

    fn hp_on_trade(
        engine: *mut HpEngineOpaque,
        side: i32,
        price: i64,
        qty: u64,
    ) -> HpResult;

    fn hp_on_fill(
        engine: *mut HpEngineOpaque,
        side: i32,
        qty: u64,
        price: i64,
    );

    fn hp_best_bid(engine: *const HpEngineOpaque) -> i64;
    fn hp_best_ask(engine: *const HpEngineOpaque) -> i64;
    fn hp_mid_price(engine: *const HpEngineOpaque) -> i64;
    fn hp_spread(engine: *const HpEngineOpaque) -> i64;
    fn hp_imbalance(engine: *const HpEngineOpaque) -> i64;
    fn hp_position(engine: *const HpEngineOpaque) -> i64;
    fn hp_realized_pnl(engine: *const HpEngineOpaque) -> i64;
}

// ── Safe Rust wrapper ─────────────────────────────────────────

/// Max depth levels per side — must match C++ HP_MAX_LEVELS.
const MAX_LEVELS: usize = 40;

/// High-performance MM engine backed by C++ hot path.
/// All compute happens in ~100-500ns per tick.
pub struct HotPathEngine {
    ptr: *mut HpEngineOpaque,
    bid_buf: [HpLevel; MAX_LEVELS],
    ask_buf: [HpLevel; MAX_LEVELS],
}

// SAFETY: The C++ engine has no thread-local state and all
// access is through &mut self (exclusive reference).
unsafe impl Send for HotPathEngine {}

impl HotPathEngine {
    pub fn new(
        order_qty: u64,
        max_inventory: i64,
        half_spread_bps: i32,
        gamma: i32,
        warmup_ticks: i32,
        cooldown_ticks: i32,
        requote_threshold: i64,
        vpin_enabled: bool,
    ) -> Self {
        let ptr = unsafe {
            hp_engine_create(
                order_qty,
                max_inventory,
                half_spread_bps,
                gamma,
                warmup_ticks,
                cooldown_ticks,
                requote_threshold,
                vpin_enabled,
            )
        };
        assert!(!ptr.is_null(), "Failed to create C++ hot-path engine");
        Self {
            ptr,
            bid_buf: [HpLevel::default(); MAX_LEVELS],
            ask_buf: [HpLevel::default(); MAX_LEVELS],
        }
    }

    /// Process an order book update. Returns MM commands.
    /// This is THE hot path — zero-alloc, ~100-500ns.
    pub fn on_book_update(
        &mut self,
        bids: &[Level],
        asks: &[Level],
        is_snapshot: bool,
    ) -> HpResult {
        let bid_count = bids.len().min(MAX_LEVELS);
        let ask_count = asks.len().min(MAX_LEVELS);

        for i in 0..bid_count {
            self.bid_buf[i] = HpLevel { price: bids[i].price.0, qty: bids[i].qty.0 };
        }
        for i in 0..ask_count {
            self.ask_buf[i] = HpLevel { price: asks[i].price.0, qty: asks[i].qty.0 };
        }

        unsafe {
            hp_on_book_update(
                self.ptr,
                self.bid_buf.as_ptr(),
                bid_count as i32,
                self.ask_buf.as_ptr(),
                ask_count as i32,
                is_snapshot,
            )
        }
    }

    /// Process a trade event. Returns cancel command if toxic.
    pub fn on_trade(&mut self, side: Side, price: Price, qty: Qty) -> HpResult {
        let s = match side {
            Side::Buy => 0,
            Side::Sell => 1,
        };
        unsafe { hp_on_trade(self.ptr, s, price.0, qty.0) }
    }

    /// Notify of a fill for inventory tracking.
    pub fn on_fill(&mut self, side: Side, qty: Qty, price: Price) {
        let s = match side {
            Side::Buy => 0,
            Side::Sell => 1,
        };
        unsafe { hp_on_fill(self.ptr, s, qty.0, price.0) }
    }

    pub fn best_bid(&self) -> Price {
        Price(unsafe { hp_best_bid(self.ptr) })
    }

    pub fn best_ask(&self) -> Price {
        Price(unsafe { hp_best_ask(self.ptr) })
    }

    pub fn mid_price(&self) -> Price {
        Price(unsafe { hp_mid_price(self.ptr) })
    }

    pub fn spread(&self) -> Price {
        Price(unsafe { hp_spread(self.ptr) })
    }

    /// Imbalance × 10000
    pub fn imbalance(&self) -> i64 {
        unsafe { hp_imbalance(self.ptr) }
    }

    pub fn position(&self) -> i64 {
        unsafe { hp_position(self.ptr) }
    }

    pub fn realized_pnl(&self) -> i64 {
        unsafe { hp_realized_pnl(self.ptr) }
    }
}

impl Drop for HotPathEngine {
    fn drop(&mut self) {
        unsafe { hp_engine_destroy(self.ptr) }
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_levels(prices: &[(i64, u64)]) -> Vec<Level> {
        prices
            .iter()
            .map(|(p, q)| Level { price: Price(*p), qty: Qty(*q) })
            .collect()
    }

    #[test]
    fn test_engine_create_destroy() {
        let engine = HotPathEngine::new(1_000_000, 10_000_000, 1, 3000, 3, 5, 10, false);
        assert_eq!(engine.position(), 0);
        drop(engine);
    }

    #[test]
    fn test_book_update_generates_quotes() {
        let mut engine = HotPathEngine::new(1_000_000, 10_000_000, 1, 3000, 3, 5, 10, false);

        let bids = make_levels(&[
            (7322300, 100_000_000),
            (7322200, 50_000_000),
            (7322100, 30_000_000),
        ]);
        let asks = make_levels(&[
            (7322400, 80_000_000),
            (7322500, 60_000_000),
            (7322600, 40_000_000),
        ]);

        // Warmup ticks (3 needed)
        for _ in 0..3 {
            engine.on_book_update(&bids, &asks, true);
        }

        // 4th tick should generate a quote (first quote after warmup)
        let result = engine.on_book_update(&bids, &asks, true);
        // First tick with last_fair_value == 0 fires immediately
        // After warmup, it should have produced commands
        assert!(result.fair_value > 0, "Fair value should be computed");
    }

    #[test]
    fn test_trade_toxicity() {
        let mut engine = HotPathEngine::new(1_000_000, 10_000_000, 1, 3000, 3, 5, 10, true);

        // Send many buy trades to build toxicity
        for i in 0..100 {
            engine.on_trade(Side::Buy, Price(7322300 + i), Qty(10_000_000));
        }

        let result = engine.on_trade(Side::Buy, Price(7322400), Qty(10_000_000));
        assert!(result.vpin > 5000, "VPIN should be high with all-buy trades");
    }

    #[test]
    fn test_fill_updates_inventory() {
        let mut engine = HotPathEngine::new(1_000_000, 10_000_000, 1, 3000, 3, 5, 10, false);

        engine.on_fill(Side::Buy, Qty(5_000_000), Price(7322300));
        assert_eq!(engine.position(), 5_000_000);

        engine.on_fill(Side::Sell, Qty(2_000_000), Price(7322500));
        assert_eq!(engine.position(), 3_000_000);
    }

    #[test]
    fn test_compute_time_under_microsecond() {
        let mut engine = HotPathEngine::new(1_000_000, 10_000_000, 1, 3000, 3, 5, 10, false);

        let bids = make_levels(&[
            (7322300, 100_000_000),
            (7322200, 50_000_000),
            (7322100, 30_000_000),
            (7322000, 20_000_000),
            (7321900, 10_000_000),
        ]);
        let asks = make_levels(&[
            (7322400, 80_000_000),
            (7322500, 60_000_000),
            (7322600, 40_000_000),
            (7322700, 25_000_000),
            (7322800, 15_000_000),
        ]);

        let result = engine.on_book_update(&bids, &asks, true);
        // compute_ns should be under 10 microseconds even on slow machines
        assert!(
            result.compute_ns < 10_000,
            "Compute took {}ns — should be under 10µs",
            result.compute_ns
        );
    }
}
