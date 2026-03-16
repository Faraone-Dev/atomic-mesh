use atomic_core::event::Event;
use atomic_core::types::{OrderId, OrderType, Price, Qty, Side, Symbol, TimeInForce, Venue};
use atomic_orderbook::OrderBook;

/// Command that a strategy can emit.
#[derive(Debug, Clone)]
pub enum StrategyCommand {
    PlaceOrder {
        symbol: Symbol,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
        time_in_force: TimeInForce,
        venue: Venue,
    },
    CancelOrder {
        order_id: OrderId,
    },
    CancelAll {
        symbol: Option<Symbol>,
    },
}

/// Context passed to strategy on each event.
pub struct StrategyContext<'a> {
    pub books: &'a std::collections::HashMap<String, OrderBook>,
    pub positions: &'a std::collections::HashMap<String, atomic_core::types::Position>,
    pub seq: u64,
    pub timestamp: u64,
}

/// Core strategy trait. All strategies implement this.
/// Strategies MUST be deterministic: same events → same commands.
/// No random, no wall-clock, no I/O in the hot path.
pub trait Strategy: Send {
    /// Strategy identifier.
    fn id(&self) -> &str;

    /// Called for every sequenced event. Returns commands to execute.
    fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Vec<StrategyCommand>;

    /// Called on strategy startup (after snapshot load).
    fn on_start(&mut self) {}

    /// Called on strategy shutdown.
    fn on_stop(&mut self) {}

    /// Serialize strategy state for snapshotting.
    fn snapshot(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore strategy state from snapshot.
    fn restore(&mut self, _data: &[u8]) {}
}
