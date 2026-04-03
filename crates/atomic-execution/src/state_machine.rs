use atomic_core::types::{OrderId, OrderType, Price, Qty, Side, Symbol, TimeInForce, Venue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Order lifecycle states.
/// Valid transitions:
///   New → Ack → PartialFill → Filled
///   New → Ack → Canceled
///   New → Rejected
///   Ack → PartialFill → Canceled
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderState {
    New,
    Ack,
    PartialFill,
    Filled,
    Canceled,
    Rejected,
}

impl OrderState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Rejected)
    }

    pub fn can_transition_to(self, next: OrderState) -> bool {
        matches!(
            (self, next),
            (Self::New, Self::Ack)
                | (Self::New, Self::Rejected)
                | (Self::Ack, Self::PartialFill)
                | (Self::Ack, Self::Filled)
                | (Self::Ack, Self::Canceled)
                | (Self::PartialFill, Self::PartialFill)
                | (Self::PartialFill, Self::Filled)
                | (Self::PartialFill, Self::Canceled)
        )
    }
}

/// Live order tracking entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveOrder {
    pub order_id: OrderId,
    pub exchange_order_id: Option<String>,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Price,
    pub original_qty: Qty,
    pub filled_qty: Qty,
    pub remaining_qty: Qty,
    pub time_in_force: TimeInForce,
    pub venue: Venue,
    pub state: OrderState,
    pub created_at: u64,
    pub updated_at: u64,
}

impl LiveOrder {
    pub fn new(
        order_id: OrderId,
        symbol: Symbol,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
        time_in_force: TimeInForce,
        venue: Venue,
        timestamp: u64,
    ) -> Self {
        Self {
            order_id,
            exchange_order_id: None,
            symbol,
            side,
            order_type,
            price,
            original_qty: qty,
            filled_qty: Qty::ZERO,
            remaining_qty: qty,
            time_in_force,
            venue,
            state: OrderState::New,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    pub fn fill_pct(&self) -> f64 {
        if self.original_qty.0 == 0 {
            return 0.0;
        }
        self.filled_qty.0 as f64 / self.original_qty.0 as f64 * 100.0
    }
}

/// Execution engine: tracks all orders and enforces state machine transitions.
pub struct ExecutionEngine {
    orders: HashMap<String, LiveOrder>,
    next_local_id: u64,
}

impl ExecutionEngine {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
            next_local_id: 1,
        }
    }

    pub fn next_order_id(&mut self, venue: Venue) -> OrderId {
        let id = OrderId::new(venue, self.next_local_id);
        self.next_local_id += 1;
        id
    }

    /// Register a new order.
    pub fn register_order(&mut self, order: LiveOrder) -> Result<(), String> {
        if self.orders.contains_key(&order.order_id.0) {
            return Err(format!("order already exists: {}", order.order_id));
        }
        self.orders.insert(order.order_id.0.clone(), order);
        Ok(())
    }

    /// Transition order to Ack state.
    pub fn on_ack(&mut self, order_id: &str, exchange_id: String, ts: u64) -> Result<(), String> {
        let order = self.orders.get_mut(order_id).ok_or("order not found")?;
        if !order.state.can_transition_to(OrderState::Ack) {
            return Err(format!(
                "invalid transition: {:?} -> Ack",
                order.state
            ));
        }
        order.state = OrderState::Ack;
        order.exchange_order_id = Some(exchange_id);
        order.updated_at = ts;
        Ok(())
    }

    /// Process a fill (partial or complete).
    pub fn on_fill(
        &mut self,
        order_id: &str,
        fill_qty: Qty,
        _fill_price: Price,
        ts: u64,
    ) -> Result<bool, String> {
        let order = self.orders.get_mut(order_id).ok_or("order not found")?;

        let new_filled = order.filled_qty.0 + fill_qty.0;
        let is_complete = new_filled >= order.original_qty.0;

        let target_state = if is_complete {
            OrderState::Filled
        } else {
            OrderState::PartialFill
        };

        if !order.state.can_transition_to(target_state) {
            return Err(format!(
                "invalid transition: {:?} -> {:?}",
                order.state, target_state
            ));
        }

        order.filled_qty = Qty(new_filled);
        order.remaining_qty = Qty(order.original_qty.0.saturating_sub(new_filled));
        order.state = target_state;
        order.updated_at = ts;

        Ok(is_complete)
    }

    /// Cancel an order.
    pub fn on_cancel(&mut self, order_id: &str, ts: u64) -> Result<(), String> {
        let order = self.orders.get_mut(order_id).ok_or("order not found")?;
        if !order.state.can_transition_to(OrderState::Canceled) {
            return Err(format!(
                "invalid transition: {:?} -> Canceled",
                order.state
            ));
        }
        order.state = OrderState::Canceled;
        order.updated_at = ts;
        Ok(())
    }

    /// Reject an order.
    pub fn on_reject(&mut self, order_id: &str, ts: u64) -> Result<(), String> {
        let order = self.orders.get_mut(order_id).ok_or("order not found")?;
        if !order.state.can_transition_to(OrderState::Rejected) {
            return Err(format!(
                "invalid transition: {:?} -> Rejected",
                order.state
            ));
        }
        order.state = OrderState::Rejected;
        order.updated_at = ts;
        Ok(())
    }

    pub fn get_order(&self, order_id: &str) -> Option<&LiveOrder> {
        self.orders.get(order_id)
    }

    pub fn open_orders(&self) -> Vec<&LiveOrder> {
        self.orders
            .values()
            .filter(|o| !o.state.is_terminal())
            .collect()
    }

    /// Return OrderIds of all non-terminal orders (for cancel-on-shutdown).
    pub fn open_order_ids(&self) -> Vec<OrderId> {
        self.orders
            .values()
            .filter(|o| !o.state.is_terminal())
            .map(|o| o.order_id.clone())
            .collect()
    }

    /// Return OrderIds of open orders whose last update is older than `timeout_ns`.
    /// These are likely stuck (ack'd but no fill/cancel from exchange).
    pub fn stale_order_ids(&self, now_ns: u64, timeout_ns: u64) -> Vec<OrderId> {
        self.orders
            .values()
            .filter(|o| {
                !o.state.is_terminal()
                    && o.state != OrderState::New
                    && now_ns.saturating_sub(o.updated_at) > timeout_ns
            })
            .map(|o| o.order_id.clone())
            .collect()
    }

    pub fn all_orders(&self) -> &HashMap<String, LiveOrder> {
        &self.orders
    }

    /// Remove terminal orders (cleanup).
    pub fn gc_terminal(&mut self) -> usize {
        let before = self.orders.len();
        self.orders.retain(|_, o| !o.state.is_terminal());
        before - self.orders.len()
    }

    /// Serialize engine state deterministically for hashing.
    /// Orders are sorted by key to guarantee identical output.
    pub fn serialize_state(&self) -> Vec<u8> {
        let mut sorted_orders: Vec<&LiveOrder> = self.orders.values().collect();
        sorted_orders.sort_by(|a, b| a.order_id.0.cmp(&b.order_id.0));

        let state = EngineState {
            orders: sorted_orders.into_iter().cloned().collect(),
            next_local_id: self.next_local_id,
        };
        bincode::serialize(&state).unwrap_or_default()
    }

    /// Compute xxHash3 of current engine state.
    pub fn state_hash(&self) -> [u8; 8] {
        let data = self.serialize_state();
        atomic_core::snapshot::EngineSnapshot::compute_hash(&data)
    }

    /// Create a full snapshot at the given sequence number.
    pub fn snapshot(&self, seq: u64, timestamp: u64) -> atomic_core::snapshot::EngineSnapshot {
        let state_data = self.serialize_state();
        let state_hash = atomic_core::snapshot::EngineSnapshot::compute_hash(&state_data);
        atomic_core::snapshot::EngineSnapshot {
            seq,
            timestamp,
            state_hash,
            events_since_last: 0,
            state_data,
        }
    }

    /// Restore engine state from a snapshot.
    pub fn restore(&mut self, snapshot: &atomic_core::snapshot::EngineSnapshot) -> Result<(), String> {
        if !snapshot.verify() {
            return Err("snapshot hash verification failed".to_string());
        }
        let state: EngineState = bincode::deserialize(&snapshot.state_data)
            .map_err(|e| format!("failed to deserialize snapshot: {}", e))?;
        self.orders.clear();
        for order in state.orders {
            self.orders.insert(order.order_id.0.clone(), order);
        }
        self.next_local_id = state.next_local_id;
        Ok(())
    }

    /// Process an event and update internal state accordingly.
    /// Returns Ok(true) if the event was relevant and state changed.
    pub fn process_event(&mut self, event: &atomic_core::event::Event) -> Result<bool, String> {
        match &event.payload {
            atomic_core::event::EventPayload::OrderNew(e) => {
                let order = LiveOrder::new(
                    e.order_id.clone(),
                    e.symbol.clone(),
                    e.side,
                    e.order_type,
                    e.price,
                    e.qty,
                    e.time_in_force,
                    e.venue,
                    event.timestamp,
                );
                self.register_order(order)?;
                Ok(true)
            }
            atomic_core::event::EventPayload::OrderAck(e) => {
                self.on_ack(&e.order_id.0, e.exchange_order_id.clone(), event.timestamp)?;
                Ok(true)
            }
            atomic_core::event::EventPayload::OrderFill(e) | atomic_core::event::EventPayload::OrderPartialFill(e) => {
                self.on_fill(&e.order_id.0, e.qty, e.price, event.timestamp)?;
                Ok(true)
            }
            atomic_core::event::EventPayload::OrderCancel(e) => {
                self.on_cancel(&e.order_id.0, event.timestamp)?;
                Ok(true)
            }
            atomic_core::event::EventPayload::OrderReject(e) => {
                self.on_reject(&e.order_id.0, event.timestamp)?;
                Ok(true)
            }
            _ => Ok(false), // Not an execution event
        }
    }
}

/// Serializable engine state for snapshot/restore.
#[derive(Serialize, Deserialize)]
struct EngineState {
    orders: Vec<LiveOrder>,
    next_local_id: u64,
}

/// Periodic state verifier.
/// Tracks event count and triggers verification at configurable intervals.
pub struct StateVerifier {
    interval: u64,
    events_since_verify: u64,
    last_verified_seq: u64,
    last_hash: [u8; 8],
}

impl StateVerifier {
    pub fn new(interval: u64) -> Self {
        Self {
            interval,
            events_since_verify: 0,
            last_verified_seq: 0,
            last_hash: [0u8; 8],
        }
    }

    /// Tick the verifier. Returns true if it's time to verify.
    pub fn tick(&mut self) -> bool {
        self.events_since_verify += 1;
        self.events_since_verify >= self.interval
    }

    /// Record a verification result.
    pub fn on_verified(&mut self, seq: u64, hash: [u8; 8]) {
        self.last_verified_seq = seq;
        self.last_hash = hash;
        self.events_since_verify = 0;
    }

    /// Compare a peer's hash at the same seq. Returns true if they match.
    pub fn verify_peer(&self, seq: u64, peer_hash: [u8; 8]) -> bool {
        if seq != self.last_verified_seq {
            return true; // Different seq, can't compare
        }
        self.last_hash == peer_hash
    }

    pub fn last_hash(&self) -> [u8; 8] {
        self.last_hash
    }

    pub fn last_seq(&self) -> u64 {
        self.last_verified_seq
    }
}

impl Default for ExecutionEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::Venue;

    #[test]
    fn order_lifecycle_happy_path() {
        let mut engine = ExecutionEngine::new();
        let oid = engine.next_order_id(Venue::Binance);
        let order = LiveOrder::new(
            oid.clone(),
            atomic_core::types::Symbol::new("BTC", "USDT", Venue::Binance),
            Side::Buy,
            OrderType::Limit,
            Price(60000),
            Qty(100),
            TimeInForce::GoodTilCancel,
            Venue::Binance,
            1000,
        );

        engine.register_order(order).unwrap();
        engine.on_ack(&oid.0, "EX123".into(), 1001).unwrap();
        let complete = engine.on_fill(&oid.0, Qty(50), Price(60000), 1002).unwrap();
        assert!(!complete);
        assert_eq!(engine.get_order(&oid.0).unwrap().state, OrderState::PartialFill);

        let complete = engine.on_fill(&oid.0, Qty(50), Price(60000), 1003).unwrap();
        assert!(complete);
        assert_eq!(engine.get_order(&oid.0).unwrap().state, OrderState::Filled);
    }

    #[test]
    fn invalid_transition_rejected() {
        let mut engine = ExecutionEngine::new();
        let oid = engine.next_order_id(Venue::Binance);
        let order = LiveOrder::new(
            oid.clone(),
            atomic_core::types::Symbol::new("BTC", "USDT", Venue::Binance),
            Side::Buy,
            OrderType::Limit,
            Price(60000),
            Qty(100),
            TimeInForce::GoodTilCancel,
            Venue::Binance,
            1000,
        );

        engine.register_order(order).unwrap();
        // Can't fill without ack
        assert!(engine.on_fill(&oid.0, Qty(50), Price(60000), 1001).is_err());
    }

    #[test]
    fn stale_order_detection() {
        let mut engine = ExecutionEngine::new();
        let oid = engine.next_order_id(Venue::Binance);
        let order = LiveOrder::new(
            oid.clone(),
            atomic_core::types::Symbol::new("BTC", "USDT", Venue::Binance),
            Side::Buy,
            OrderType::Limit,
            Price(60000),
            Qty(100),
            TimeInForce::GoodTilCancel,
            Venue::Binance,
            1000, // created_at = 1000ns
        );
        engine.register_order(order).unwrap();
        engine.on_ack(&oid.0, "EX1".into(), 2000).unwrap(); // updated_at = 2000ns

        // Not stale yet (only 3000ns elapsed, timeout = 10_000ns)
        let stale = engine.stale_order_ids(5000, 10_000);
        assert!(stale.is_empty());

        // Now stale (13000ns since update, timeout = 10_000ns)
        let stale = engine.stale_order_ids(13_000, 10_000);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0], oid);

        // Filled orders are not stale
        engine.on_fill(&oid.0, Qty(100), Price(60000), 15_000).unwrap();
        let stale = engine.stale_order_ids(100_000, 10_000);
        assert!(stale.is_empty());
    }
}
