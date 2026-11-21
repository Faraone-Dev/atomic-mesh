use atomic_core::types::Side;
use flowtox::Vpin;

/// Trade-flow toxicity tracker (VPIN) + short-term volatility.
///
/// Detects "informed" order flow with volume-bucketed VPIN (Easley, Lopez de
/// Prado, O'Hara 2012): the mean buy/sell imbalance over the last N
/// equal-volume buckets, computed by `flowtox`.
/// When VPIN is high → widen spread or pull quotes entirely.
///
/// Also tracks volatility via EMA of absolute trade-to-trade price changes.
pub struct ToxicityTracker {
    /// Volume-clock VPIN over the last `n_buckets` buckets.
    vpin: Vpin,
    /// EMA of |Δprice| (volatility proxy, in pipettes).
    volatility_ema: i64,
    last_price: i64,
    /// EMA α, scaled × 10 000 (e.g., 500 = 0.05).
    vol_alpha: i64,
    /// VPIN threshold (× 10 000). Above → toxic.
    threshold: i64,
}

impl ToxicityTracker {
    pub fn new(bucket_volume: u64, n_buckets: usize, vol_alpha: i64, threshold: i64) -> Self {
        Self {
            vpin: Vpin::new(bucket_volume.max(1) as f64, n_buckets.max(1))
                .expect("bucket volume and bucket count are positive"),
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

        let side = match side {
            Side::Buy => flowtox::Side::Buy,
            Side::Sell => flowtox::Side::Sell,
        };
        self.vpin.update_side(side, qty as f64);
    }

    /// VPIN = mean of |buy_vol − sell_vol| / bucket_vol over the last N
    /// volume buckets, scaled × 10 000. Zero until N buckets have filled.
    pub fn vpin(&self) -> i64 {
        self.vpin
            .value()
            .map_or(0, |v| (v * 10000.0).round() as i64)
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
        let mut t = ToxicityTracker::new(200, 1, 500, 6000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Sell, 100, 1001);
        assert_eq!(t.vpin(), 0);
    }

    #[test]
    fn vpin_one_sided_is_max() {
        let mut t = ToxicityTracker::new(100, 4, 500, 6000);
        for _ in 0..10 {
            t.on_trade(Side::Buy, 100, 1000);
        }
        assert_eq!(t.vpin(), 10000);
        assert!(t.is_toxic());
    }

    #[test]
    fn volatility_increases_on_price_move() {
        let mut t = ToxicityTracker::new(100, 4, 5000, 6000);
        t.on_trade(Side::Buy, 100, 1000);
        t.on_trade(Side::Buy, 100, 1100); // Δ = 100
        assert!(t.volatility() > 0);
    }

    #[test]
    fn spread_multiplier_baseline() {
        let t = ToxicityTracker::new(100, 4, 500, 6000);
        assert_eq!(t.spread_multiplier(), 10000); // 1.0×
    }

    #[test]
    fn one_sided_runs_are_toxic_even_when_totals_balance() {
        // 200 bought, then 200 sold: total buy == total sell, so a plain
        // window imbalance reads 0. Every 100-unit bucket is one-sided.
        let mut t = ToxicityTracker::new(100, 4, 500, 6000);
        for _ in 0..2 {
            t.on_trade(Side::Buy, 100, 1000);
        }
        for _ in 0..2 {
            t.on_trade(Side::Sell, 100, 1000);
        }
        assert_eq!(t.vpin(), 10000);
        assert!(t.is_toxic());
    }

    #[test]
    fn buckets_follow_volume_not_trade_count() {
        // One 300-unit buy spans three buckets; ten 10-unit sells fill one.
        let mut t = ToxicityTracker::new(100, 4, 500, 6000);
        t.on_trade(Side::Buy, 300, 1000);
        assert_eq!(t.vpin(), 0); // 3 of 4 buckets: not ready yet
        for _ in 0..10 {
            t.on_trade(Side::Sell, 10, 1000);
        }
        assert_eq!(t.vpin(), 10000);
    }
}
