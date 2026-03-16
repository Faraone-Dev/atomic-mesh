use atomic_core::types::{OrderType, Price, Qty, Side, Symbol, TimeInForce, Venue};
use atomic_orderbook::OrderBook;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Routing algorithm for splitting orders across venues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingAlgo {
    /// Send entire order to best venue.
    BestVenue,
    /// Volume-Weighted Average Price: split across venues proportional to depth.
    VWAP,
    /// Time-Weighted Average Price: split across time slices.
    TWAP { slices: u32, interval_ms: u64 },
    /// Sweep liquidity across all venues simultaneously.
    LiquiditySweep,
}

/// A routed order slice — part of a parent order sent to a specific venue.
#[derive(Debug, Clone)]
pub struct OrderSlice {
    pub venue: Venue,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Price,
    pub qty: Qty,
    pub time_in_force: TimeInForce,
}

/// Smart Order Router: splits orders across venues based on liquidity.
pub struct SmartOrderRouter {
    books: HashMap<(String, Venue), OrderBook>,
}

impl SmartOrderRouter {
    pub fn new() -> Self {
        Self {
            books: HashMap::new(),
        }
    }

    pub fn update_book(&mut self, symbol: &str, venue: Venue, book: OrderBook) {
        self.books.insert((symbol.to_string(), venue), book);
    }

    /// Route an order using the specified algorithm.
    pub fn route(
        &self,
        symbol: &Symbol,
        side: Side,
        qty: Qty,
        algo: RoutingAlgo,
    ) -> Vec<OrderSlice> {
        match algo {
            RoutingAlgo::BestVenue => self.route_best_venue(symbol, side, qty),
            RoutingAlgo::VWAP => self.route_vwap(symbol, side, qty),
            RoutingAlgo::TWAP { slices, .. } => self.route_twap(symbol, side, qty, slices),
            RoutingAlgo::LiquiditySweep => self.route_sweep(symbol, side, qty),
        }
    }

    fn route_best_venue(&self, symbol: &Symbol, side: Side, qty: Qty) -> Vec<OrderSlice> {
        let pair = symbol.pair();
        let mut best_price: Option<(Venue, Price)> = None;

        for ((sym, venue), book) in &self.books {
            if *sym != pair {
                continue;
            }
            let level = match side {
                Side::Buy => book.best_ask(),
                Side::Sell => book.best_bid(),
            };
            if let Some(level) = level {
                match &best_price {
                    None => best_price = Some((*venue, level.price)),
                    Some((_, bp)) => {
                        let is_better = match side {
                            Side::Buy => level.price.0 < bp.0,
                            Side::Sell => level.price.0 > bp.0,
                        };
                        if is_better {
                            best_price = Some((*venue, level.price));
                        }
                    }
                }
            }
        }

        if let Some((venue, price)) = best_price {
            vec![OrderSlice {
                venue,
                symbol: symbol.clone(),
                side,
                order_type: OrderType::Limit,
                price,
                qty,
                time_in_force: TimeInForce::ImmediateOrCancel,
            }]
        } else {
            Vec::new()
        }
    }

    fn route_vwap(&self, symbol: &Symbol, side: Side, qty: Qty) -> Vec<OrderSlice> {
        let pair = symbol.pair();
        let mut venue_depths: Vec<(Venue, u64, Price)> = Vec::new();

        for ((sym, venue), book) in &self.books {
            if *sym != pair {
                continue;
            }
            let (depth, price) = match side {
                Side::Buy => (book.ask_depth(), book.best_ask().map(|l| l.price)),
                Side::Sell => (book.bid_depth(), book.best_bid().map(|l| l.price)),
            };
            if let Some(price) = price {
                venue_depths.push((*venue, depth, price));
            }
        }

        if venue_depths.is_empty() {
            return Vec::new();
        }

        let total_depth: u64 = venue_depths.iter().map(|(_, d, _)| d).sum();
        if total_depth == 0 {
            return Vec::new();
        }

        venue_depths
            .iter()
            .filter_map(|(venue, depth, price)| {
                let proportion = *depth as f64 / total_depth as f64;
                let slice_qty = (qty.0 as f64 * proportion).round() as u64;
                if slice_qty == 0 {
                    return None;
                }
                Some(OrderSlice {
                    venue: *venue,
                    symbol: symbol.clone(),
                    side,
                    order_type: OrderType::Limit,
                    price: *price,
                    qty: Qty(slice_qty),
                    time_in_force: TimeInForce::ImmediateOrCancel,
                })
            })
            .collect()
    }

    fn route_twap(&self, symbol: &Symbol, side: Side, qty: Qty, slices: u32) -> Vec<OrderSlice> {
        // TWAP: divide qty equally across slices (execution timing handled by caller)
        let slice_qty = qty.0 / slices as u64;
        if slice_qty == 0 {
            return Vec::new();
        }

        // For now, route each slice to best venue
        let mut result = Vec::new();
        for _ in 0..slices {
            let mut routed = self.route_best_venue(symbol, side, Qty(slice_qty));
            result.append(&mut routed);
        }
        result
    }

    fn route_sweep(&self, symbol: &Symbol, side: Side, qty: Qty) -> Vec<OrderSlice> {
        let pair = symbol.pair();
        let mut all_levels: Vec<(Venue, Price, Qty)> = Vec::new();

        for ((sym, venue), book) in &self.books {
            if *sym != pair {
                continue;
            }
            let levels = match side {
                Side::Buy => book.top_asks(20),
                Side::Sell => book.top_bids(20),
            };
            for level in levels {
                all_levels.push((*venue, level.price, level.qty));
            }
        }

        // Sort by price: ascending for buys, descending for sells
        match side {
            Side::Buy => all_levels.sort_by_key(|(_, p, _)| p.0),
            Side::Sell => all_levels.sort_by_key(|(_, p, _)| std::cmp::Reverse(p.0)),
        }

        let mut remaining = qty.0 as i64;
        let mut slices: Vec<OrderSlice> = Vec::new();

        for (venue, price, available) in all_levels {
            if remaining <= 0 {
                break;
            }
            let fill = std::cmp::min(remaining, available.0 as i64) as u64;
            slices.push(OrderSlice {
                venue,
                symbol: symbol.clone(),
                side,
                order_type: OrderType::Limit,
                price,
                qty: Qty(fill),
                time_in_force: TimeInForce::ImmediateOrCancel,
            });
            remaining -= fill as i64;
        }

        slices
    }
}

impl Default for SmartOrderRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::{Level, Price, Qty, Side, Symbol, Venue};
    use atomic_orderbook::OrderBook;

    fn sym(venue: Venue) -> Symbol {
        Symbol::new("BTC", "USDT", venue)
    }

    fn book_with_levels(symbol: Symbol, bids: &[(i64, u64)], asks: &[(i64, u64)]) -> OrderBook {
        let mut book = OrderBook::new(symbol);
        let bid_levels: Vec<Level> = bids.iter().map(|(p, q)| Level { price: Price(*p), qty: Qty(*q) }).collect();
        let ask_levels: Vec<Level> = asks.iter().map(|(p, q)| Level { price: Price(*p), qty: Qty(*q) }).collect();
        book.apply_snapshot(&bid_levels, &ask_levels, 1, 0);
        book
    }

    #[test]
    fn best_venue_picks_lowest_ask_for_buy() {
        let mut router = SmartOrderRouter::new();
        let s1 = sym(Venue::Binance);
        let s2 = sym(Venue::Bybit);
        router.update_book(&s1.pair(), Venue::Binance, book_with_levels(s1.clone(), &[(99, 10)], &[(101, 10)]));
        router.update_book(&s2.pair(), Venue::Bybit, book_with_levels(s2.clone(), &[(99, 10)], &[(100, 10)]));

        let slices = router.route(&s1, Side::Buy, Qty(5), RoutingAlgo::BestVenue);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].venue, Venue::Bybit);
        assert_eq!(slices[0].price, Price(100));
    }

    #[test]
    fn best_venue_picks_highest_bid_for_sell() {
        let mut router = SmartOrderRouter::new();
        let s1 = sym(Venue::Binance);
        let s2 = sym(Venue::Bybit);
        router.update_book(&s1.pair(), Venue::Binance, book_with_levels(s1.clone(), &[(102, 10)], &[(105, 10)]));
        router.update_book(&s2.pair(), Venue::Bybit, book_with_levels(s2.clone(), &[(100, 10)], &[(105, 10)]));

        let slices = router.route(&s1, Side::Sell, Qty(5), RoutingAlgo::BestVenue);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].venue, Venue::Binance);
        assert_eq!(slices[0].price, Price(102));
    }

    #[test]
    fn vwap_splits_proportional_to_depth() {
        let mut router = SmartOrderRouter::new();
        let s1 = sym(Venue::Binance);
        let s2 = sym(Venue::Bybit);
        // Binance: 30 depth, Bybit: 10 depth → 75%/25% split
        router.update_book(&s1.pair(), Venue::Binance, book_with_levels(s1.clone(), &[(99, 30)], &[(101, 30)]));
        router.update_book(&s2.pair(), Venue::Bybit, book_with_levels(s2.clone(), &[(99, 10)], &[(101, 10)]));

        let slices = router.route(&s1, Side::Buy, Qty(100), RoutingAlgo::VWAP);
        assert_eq!(slices.len(), 2);
        let total_qty: u64 = slices.iter().map(|s| s.qty.0).sum();
        assert_eq!(total_qty, 100);
    }

    #[test]
    fn twap_splits_into_equal_slices() {
        let mut router = SmartOrderRouter::new();
        let s = sym(Venue::Binance);
        router.update_book(&s.pair(), Venue::Binance, book_with_levels(s.clone(), &[(99, 100)], &[(101, 100)]));

        let slices = router.route(&s, Side::Buy, Qty(100), RoutingAlgo::TWAP { slices: 4, interval_ms: 1000 });
        assert_eq!(slices.len(), 4);
        for slice in &slices {
            assert_eq!(slice.qty, Qty(25));
        }
    }

    #[test]
    fn sweep_fills_across_levels() {
        let mut router = SmartOrderRouter::new();
        let s = sym(Venue::Binance);
        router.update_book(&s.pair(), Venue::Binance, book_with_levels(s.clone(), &[(99, 50)], &[(100, 20), (101, 30)]));

        let slices = router.route(&s, Side::Buy, Qty(40), RoutingAlgo::LiquiditySweep);
        assert!(!slices.is_empty());
        let total_qty: u64 = slices.iter().map(|s| s.qty.0).sum();
        assert_eq!(total_qty, 40);
    }

    #[test]
    fn empty_book_returns_no_slices() {
        let router = SmartOrderRouter::new();
        let s = sym(Venue::Binance);
        let slices = router.route(&s, Side::Buy, Qty(100), RoutingAlgo::BestVenue);
        assert!(slices.is_empty());
    }
}
