use atomic_core::types::{Level, Price};

/// Compute microprice from top-of-book.
///
/// Microprice = (ask_price × bid_vol + bid_price × ask_vol) / (bid_vol + ask_vol)
///
/// Unlike the simple midpoint, microprice weighs prices by the *opposite* side's volume,
/// reflecting where the "true" fair value sits given the supply/demand imbalance.
pub fn compute(best_bid: &Level, best_ask: &Level) -> Price {
    let bv = best_bid.qty.0 as i128;
    let av = best_ask.qty.0 as i128;
    let total = bv + av;
    if total == 0 {
        return Price((best_bid.price.0 + best_ask.price.0) / 2);
    }
    let micro = (best_ask.price.0 as i128 * bv + best_bid.price.0 as i128 * av) / total;
    Price(micro as i64)
}

/// Multi-level weighted microprice — uses top `depth` levels.
/// Deeper levels get linearly decreasing weight: w(i) = depth - i.
pub fn compute_weighted(bids: &[Level], asks: &[Level], depth: usize) -> Price {
    if bids.is_empty() || asks.is_empty() {
        return Price(0);
    }
    let d = depth.min(bids.len()).min(asks.len());
    if d == 0 {
        return compute(&bids[0], &asks[0]);
    }

    let mut numerator: i128 = 0;
    let mut denominator: i128 = 0;

    for i in 0..d {
        let w = (d - i) as i128;
        let bv = bids[i].qty.0 as i128;
        let av = asks[i].qty.0 as i128;
        numerator += (asks[i].price.0 as i128 * bv + bids[i].price.0 as i128 * av) * w;
        denominator += (bv + av) * w;
    }

    if denominator == 0 {
        return Price((bids[0].price.0 + asks[0].price.0) / 2);
    }
    Price((numerator / denominator) as i64)
}

/// Order book volume imbalance: (bid_vol − ask_vol) / (bid_vol + ask_vol).
/// Returns value in [−10000, 10000] (scaled ×10 000).
/// Positive → more bids (bullish), negative → more asks (bearish).
pub fn imbalance(bids: &[Level], asks: &[Level], depth: usize) -> i64 {
    let d = depth.min(bids.len()).min(asks.len());
    if d == 0 {
        return 0;
    }
    let mut bv: i128 = 0;
    let mut av: i128 = 0;
    for i in 0..d {
        bv += bids[i].qty.0 as i128;
        av += asks[i].qty.0 as i128;
    }
    let total = bv + av;
    if total == 0 {
        return 0;
    }
    ((bv - av) * 10000 / total) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::{Qty};

    fn lvl(price: i64, qty: u64) -> Level {
        Level { price: Price(price), qty: Qty(qty) }
    }

    #[test]
    fn microprice_equal_volumes() {
        let bid = lvl(100_00, 500);
        let ask = lvl(101_00, 500);
        let mp = compute(&bid, &ask);
        // equal volumes → midpoint
        assert_eq!(mp.0, 100_50);
    }

    #[test]
    fn microprice_heavier_bid() {
        let bid = lvl(100_00, 900);
        let ask = lvl(101_00, 100);
        let mp = compute(&bid, &ask);
        // more bid volume → microprice closer to ask
        assert!(mp.0 > 100_50);
    }

    #[test]
    fn imbalance_balanced() {
        let bids = vec![lvl(100_00, 500)];
        let asks = vec![lvl(101_00, 500)];
        assert_eq!(imbalance(&bids, &asks, 5), 0);
    }

    #[test]
    fn imbalance_bullish() {
        let bids = vec![lvl(100_00, 800)];
        let asks = vec![lvl(101_00, 200)];
        let imb = imbalance(&bids, &asks, 5);
        assert_eq!(imb, 6000); // (800-200)/1000 * 10000
    }

    #[test]
    fn weighted_empty_returns_zero() {
        assert_eq!(compute_weighted(&[], &[], 5).0, 0);
    }
}
