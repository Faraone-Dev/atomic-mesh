use atomic_core::types::Side;
use std::collections::VecDeque;

/// Trade-flow toxicity tracker (VPIN-lite) + short-term volatility.
///
/// Detects "informed" order flow by measuring the imbalance between
/// buy-initiated and sell-initiated volume in a rolling window.
/// When VPIN is high → widen spread or pull quotes entirely.
///
/// Also tracks volatility via EMA of absolute trade-to-trade price changes.
pub struct ToxicityTracker {
    /// Rolling window of (side, qty).
    window: VecDeque<(Side, u64)>,
    max_window: usize,
    buy_volume: u64,
    sell_volume: u64,
    /// EMA of |Δprice| (volatility proxy, in pipettes).
    volatility_ema: i64,
    last_price: i64,
    /// EMA α, scaled × 10 000 (e.g., 500 = 0.05).
    vol_alpha: i64,
    /// VPIN threshold (× 10 000). Above → toxic.
    threshold: i64,
}

impl ToxicityTracker {
    pub fn new(max_window: usize, vol_alpha: i64, threshold: i64) -> Self {
        Self {
            window: VecDeque::with_capacity(max_window),
            max_window,
            buy_volume: 0,
            sell_volume: 0,
            volatility_ema: 0,
            last_price: 0,
            vol_alpha,
            threshold,
        }
    }

    /// Record a new trade tick.
    pub fn on_trade(&mut self, side: Side, qty: u64, price: i64) {
        // Volatility EMA update
        if self.last_price > 0 {
            let delta = (price - self.last_price).unsigned_abs() as i64;
            self.volatility_ema =
                (delta * self.vol_alpha + self.volatility_ema * (10000 - self.vol_alpha)) / 10000;
        }
        self.last_price = price;

        // Add to rolling window
        match side {
            Side::Buy => self.buy_volume += qty,
            Side::Sell => self.sell_volume += qty,
        }
        self.window.push_back((side, qty));

        // Evict old
        while self.window.len() > self.max_window {
            if let Some((old_side, old_qty)) = self.window.pop_front() {
                match old_side {
                    Side::Buy => self.buy_volume = self.buy_volume.saturating_sub(old_qty),
                    Side::Sell => self.sell_volume = self.sell_volume.saturating_sub(old_qty),
                }
            }
        }
    }

    /// VPIN = |buy_vol − sell_vol| / total_vol, scaled × 10 000.
    pub fn vpin(&self) -> i64 {
        let total = self.buy_volume + self.sell_volume;
        if total == 0 {
            return 0;
        }
        let diff = (self.buy_volume as i64 - self.sell_volume as i64).unsigned_abs();
        (diff * 10000 / total) as i64
    }

    /// Is flow currently toxic?
    pub fn is_toxic(&self) -> bool {
        self.vpin() > self.threshold
    }

    /// Dynamic spread multiplier based on toxicity + volatility.
    /// Returns value in [10 000, 30 000] (1.0× – 3.0×, scaled × 10 000).
    pub fn spread_multiplier(&self) -> i64 {
        let tox = self.vpin(); // 0..10 000
        // Volatility component: 100 pipettes ≈ $1.00 → start widening
        let vol = (self.volatility_ema * 50).min(10000);
        (10000 + tox + vol).min(30000)
    }

    /// Current volatility estimate (pipettes).
    pub fn volatility(&self) -> i64 {
        self.volatility_ema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpin_balanced_is_zero() {
        let mut t = ToxicityTracker::new(100, 500, 6000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Sell, 100, 1001);
        assert_eq!(t.vpin(), 0);
    }

    #[test]
    fn vpin_one_sided_is_max() {
        let mut t = ToxicityTracker::new(100, 500, 6000);
        for _ in 0..10 {
            t.on_trade(Side::Buy, 100, 1000);
        }
        assert_eq!(t.vpin(), 10000);
        assert!(t.is_toxic());
    }

    #[test]
    fn volatility_increases_on_price_move() {
        let mut t = ToxicityTracker::new(100, 5000, 6000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Buy, 100, 1100); // Δ = 100
        assert!(t.volatility() > 0);
    }

    #[test]
    fn spread_multiplier_baseline() {
        let t = ToxicityTracker::new(100, 500, 6000);
        assert_eq!(t.spread_multiplier(), 10000); // 1.0×
    }

    #[test]
    fn window_eviction() {
        let mut t = ToxicityTracker::new(3, 500, 6000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Sell, 100, 1000); // oldest buy evicted
        // 2 buys + 1 sell → vpin = 1/3 * 10000 ≈ 3333
        let v = t.vpin();
        assert!(v > 3000 && v < 4000, "expected ~3333, got {}", v);
    }
}
