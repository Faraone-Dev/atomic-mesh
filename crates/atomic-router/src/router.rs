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
