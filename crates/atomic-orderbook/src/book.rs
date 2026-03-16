use atomic_core::types::{Level, Price, Qty, Side, Symbol};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// L2 order book engine using BTreeMap for sorted price levels.
/// Bids: descending (best bid = highest price).
/// Asks: ascending (best ask = lowest price).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub symbol: Symbol,
    /// Bids indexed by price (stored as-is; iterate in reverse for best-first).
    bids: BTreeMap<i64, Qty>,
    /// Asks indexed by price (iterate forward for best-first).
    asks: BTreeMap<i64, Qty>,
    pub last_update_seq: u64,
    pub last_update_ts: u64,
}

impl OrderBook {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_seq: 0,
            last_update_ts: 0,
        }
    }

    /// Apply a full snapshot (replaces all levels).
    pub fn apply_snapshot(&mut self, bids: &[Level], asks: &[Level], seq: u64, ts: u64) {
        self.bids.clear();
        self.asks.clear();
        for level in bids {
            if level.qty.0 > 0 {
                self.bids.insert(level.price.0, level.qty);
            }
        }
        for level in asks {
            if level.qty.0 > 0 {
                self.asks.insert(level.price.0, level.qty);
            }
        }
        self.last_update_seq = seq;
        self.last_update_ts = ts;
    }

    /// Apply a delta update (add/update/remove levels).
    pub fn apply_delta(&mut self, bids: &[Level], asks: &[Level], seq: u64, ts: u64) {
        for level in bids {
            if level.qty.0 == 0 {
                self.bids.remove(&level.price.0);
            } else {
                self.bids.insert(level.price.0, level.qty);
            }
        }
        for level in asks {
            if level.qty.0 == 0 {
                self.asks.remove(&level.price.0);
            } else {
                self.asks.insert(level.price.0, level.qty);
            }
        }
        self.last_update_seq = seq;
        self.last_update_ts = ts;
    }

    /// Best bid (highest price).
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.iter().next_back().map(|(&p, &q)| Level {
            price: Price(p),
            qty: q,
        })
    }

    /// Best ask (lowest price).
    pub fn best_ask(&self) -> Option<Level> {
        self.asks.iter().next().map(|(&p, &q)| Level {
            price: Price(p),
            qty: q,
        })
    }

    /// Mid price.
    pub fn mid_price(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(bid.price.midpoint(ask.price)),
            _ => None,
        }
    }

    /// Spread in price units.
    pub fn spread(&self) -> Option<i64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask.price.0 - bid.price.0),
            _ => None,
        }
    }

    /// Top N bid levels (best-first = descending price).
    pub fn top_bids(&self, n: usize) -> Vec<Level> {
        self.bids
            .iter()
            .rev()
            .take(n)
            .map(|(&p, &q)| Level { price: Price(p), qty: q })
            .collect()
    }

    /// Top N ask levels (best-first = ascending price).
    pub fn top_asks(&self, n: usize) -> Vec<Level> {
        self.asks
            .iter()
            .take(n)
            .map(|(&p, &q)| Level { price: Price(p), qty: q })
            .collect()
    }

    /// Total bid depth (sum of all bid quantities).
    pub fn bid_depth(&self) -> u64 {
        self.bids.values().map(|q| q.0).sum()
    }

    /// Total ask depth (sum of all ask quantities).
    pub fn ask_depth(&self) -> u64 {
        self.asks.values().map(|q| q.0).sum()
    }

    /// Bid depth up to a price level (inclusive).
    pub fn bid_depth_to_price(&self, price: Price) -> u64 {
        self.bids
            .range(price.0..)
            .map(|(_, q)| q.0)
            .sum()
    }

    /// Ask depth up to a price level (inclusive).
    pub fn ask_depth_to_price(&self, price: Price) -> u64 {
        self.asks
            .range(..=price.0)
            .map(|(_, q)| q.0)
            .sum()
    }

    /// Simulate market order fill: walk the book and return avg fill price.
    /// Returns (avg_price, total_filled_qty) or None if not enough liquidity.
    pub fn simulate_fill(&self, side: Side, qty: Qty) -> Option<(Price, Qty)> {
        let levels: Vec<Level> = match side {
            Side::Buy => self.top_asks(self.asks.len()),
            Side::Sell => self.top_bids(self.bids.len()),
        };

        let mut remaining = qty.0 as i64;
        let mut total_cost: i64 = 0;
        let mut total_filled: u64 = 0;

        for level in &levels {
            if remaining <= 0 {
                break;
            }
            let fill_qty = std::cmp::min(remaining, level.qty.0 as i64);
            total_cost += fill_qty * level.price.0;
            total_filled += fill_qty as u64;
            remaining -= fill_qty;
        }

        if total_filled == 0 {
            return None;
        }

        let avg_price = Price(total_cost / total_filled as i64);
        Some((avg_price, Qty(total_filled)))
    }

    pub fn bid_count(&self) -> usize {
        self.bids.len()
    }

    pub fn ask_count(&self) -> usize {
        self.asks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::Venue;

    fn test_symbol() -> Symbol {
        Symbol::new("BTC", "USDT", Venue::Binance)
    }

    #[test]
    fn snapshot_and_best() {
        let mut book = OrderBook::new(test_symbol());
        let bids = vec![
            Level { price: Price(60000), qty: Qty(100) },
            Level { price: Price(59900), qty: Qty(200) },
        ];
        let asks = vec![
            Level { price: Price(60100), qty: Qty(150) },
            Level { price: Price(60200), qty: Qty(300) },
        ];

        book.apply_snapshot(&bids, &asks, 1, 1000);

        assert_eq!(book.best_bid().unwrap().price.0, 60000);
        assert_eq!(book.best_ask().unwrap().price.0, 60100);
        assert_eq!(book.spread().unwrap(), 100);
    }

    #[test]
    fn delta_removes_level() {
        let mut book = OrderBook::new(test_symbol());
        book.apply_snapshot(
            &[Level { price: Price(100), qty: Qty(10) }],
            &[Level { price: Price(101), qty: Qty(20) }],
            1, 1000,
        );

        // Remove bid at 100
        book.apply_delta(
            &[Level { price: Price(100), qty: Qty(0) }],
            &[],
            2, 2000,
        );

        assert!(book.best_bid().is_none());
    }

    #[test]
    fn simulate_buy_fill() {
        let mut book = OrderBook::new(test_symbol());
        let asks = vec![
            Level { price: Price(100), qty: Qty(50) },
            Level { price: Price(101), qty: Qty(50) },
            Level { price: Price(102), qty: Qty(50) },
        ];
        book.apply_snapshot(&[], &asks, 1, 1000);

        let (avg_price, filled) = book.simulate_fill(Side::Buy, Qty(80)).unwrap();
        assert_eq!(filled.0, 80);
        // 50 @ 100 + 30 @ 101 = 5000 + 3030 = 8030, avg = 8030/80 = 100
        assert_eq!(avg_price.0, 100); // integer division
    }
}
