use serde::{Deserialize, Serialize};
use std::fmt;

use crate::types::{Level, OrderId, OrderType, Price, Qty, Side, Symbol, TimeInForce, Venue};

/// Strictly monotonic global sequence number.
pub type SeqNum = u64;

/// Nanosecond timestamp (from deterministic clock, NOT wall-clock in hot path).
pub type Timestamp = u64;

/// Unique identifier for the node that generated the event.
/// (Re-exported from node module — see node.rs for the canonical definition)
pub use crate::node::NodeId;

// === EVENT ===

/// Core event: every state change in the system is an Event.
/// Events are immutable, ordered by seq, and form the single source of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Global monotonic sequence number (assigned by sequencer / leader).
    pub seq: SeqNum,
    /// Logical timestamp (Lamport or deterministic monotonic nanos).
    pub timestamp: Timestamp,
    /// Node that produced this event.
    pub source: NodeId,
    /// The actual event data.
    pub payload: EventPayload,
}

impl Event {
    pub fn new(seq: SeqNum, timestamp: Timestamp, source: NodeId, payload: EventPayload) -> Self {
        Self { seq, timestamp, source, payload }
    }
}

// === EVENT PAYLOAD ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    // --- Market Data ---
    OrderBookUpdate(OrderBookUpdateEvent),
    Trade(TradeEvent),
    Ticker(TickerEvent),

    // --- Execution ---
    OrderNew(OrderNewEvent),
    OrderAck(OrderAckEvent),
    OrderFill(OrderFillEvent),
    OrderPartialFill(OrderFillEvent),
    OrderCancel(OrderCancelEvent),
    OrderReject(OrderRejectEvent),

    // --- System ---
    Snapshot(SnapshotEvent),
    Heartbeat(HeartbeatEvent),
    NodeJoin(NodeJoinEvent),
    NodeLeave(NodeLeaveEvent),

    // --- Strategy ---
    Signal(SignalEvent),
}

// === MARKET DATA EVENTS ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookUpdateEvent {
    pub symbol: Symbol,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub is_snapshot: bool,
    pub exchange_ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeEvent {
    pub symbol: Symbol,
    pub price: Price,
    pub qty: Qty,
    pub side: Side,
    pub exchange_ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickerEvent {
    pub symbol: Symbol,
    pub bid: Price,
    pub ask: Price,
    pub last: Price,
    pub volume_24h: Qty,
    pub exchange_ts: u64,
}

// === EXECUTION EVENTS ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderNewEvent {
    pub order_id: OrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Price,
    pub qty: Qty,
    pub time_in_force: TimeInForce,
    pub venue: Venue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderAckEvent {
    pub order_id: OrderId,
    pub exchange_order_id: String,
    pub venue: Venue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderFillEvent {
    pub order_id: OrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub remaining: Qty,
    pub fee: i64,
    pub is_maker: bool,
    pub venue: Venue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderCancelEvent {
    pub order_id: OrderId,
    pub reason: String,
    pub venue: Venue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRejectEvent {
    pub order_id: OrderId,
    pub reason: String,
    pub venue: Venue,
}

// === SYSTEM EVENTS ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEvent {
    pub state_hash: [u8; 8],
    pub event_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatEvent {
    pub node_id: NodeId,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeJoinEvent {
    pub node_id: NodeId,
    pub role: crate::node::NodeRole,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLeaveEvent {
    pub node_id: NodeId,
    pub reason: String,
}

// === STRATEGY EVENTS ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEvent {
    pub strategy_id: String,
    pub symbol: Symbol,
    pub side: Side,
    pub strength: f64,
    pub metadata: String,
}

// === DISPLAY ===

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[seq={} ts={}] {:?}", self.seq, self.timestamp, self.payload)
    }
}
