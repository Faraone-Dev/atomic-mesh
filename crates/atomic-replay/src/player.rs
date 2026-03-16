use atomic_core::event::Event;
use crate::log::EventLog;

/// Replay player: reads events from a log and replays them deterministically.
/// The core guarantee: replay(events.log) → identical state every time.
pub struct ReplayPlayer {
    #[allow(dead_code)]
    log: EventLog,
    current_index: usize,
    events: Vec<Event>,
    speed: ReplaySpeed,
}

#[derive(Debug, Clone, Copy)]
pub enum ReplaySpeed {
    /// Replay as fast as possible.
    Maximum,
    /// Replay at real-time speed (using original timestamps).
    RealTime,
    /// Replay at N× speed.
    Multiplier(f64),
}

impl ReplayPlayer {
    /// Load events from an event log file.
    pub fn from_file(path: &str) -> std::io::Result<Self> {
        let log = EventLog::read_only(path);
        let events = log.read_all()?;
        Ok(Self {
            log,
            current_index: 0,
            events,
            speed: ReplaySpeed::Maximum,
        })
    }

    /// Load from a pre-loaded event vector (for testing).
    pub fn from_events(events: Vec<Event>) -> Self {
        Self {
            log: EventLog::read_only(""),
            current_index: 0,
            events,
            speed: ReplaySpeed::Maximum,
        }
    }

    pub fn set_speed(&mut self, speed: ReplaySpeed) {
        self.speed = speed;
    }

    /// Get the next event without advancing.
    pub fn peek(&self) -> Option<&Event> {
        self.events.get(self.current_index)
    }

    /// Get the next event and advance.
    pub fn next(&mut self) -> Option<&Event> {
        let event = self.events.get(self.current_index)?;
        self.current_index += 1;
        Some(event)
    }

    /// Seek to a specific sequence number.
    pub fn seek_to_seq(&mut self, seq: u64) {
        self.current_index = self
            .events
            .iter()
            .position(|e| e.seq >= seq)
            .unwrap_or(self.events.len());
    }

    /// Reset to the beginning.
    pub fn reset(&mut self) {
        self.current_index = 0;
    }

    /// Get a batch of N events.
    pub fn next_batch(&mut self, n: usize) -> Vec<&Event> {
        let start = self.current_index;
        let end = std::cmp::min(start + n, self.events.len());
        self.current_index = end;
        self.events[start..end].iter().collect()
    }

    /// Total number of events.
    pub fn total_events(&self) -> usize {
        self.events.len()
    }

    /// Current position.
    pub fn position(&self) -> usize {
        self.current_index
    }

    /// Progress as percentage.
    pub fn progress_pct(&self) -> f64 {
        if self.events.is_empty() {
            return 100.0;
        }
        self.current_index as f64 / self.events.len() as f64 * 100.0
    }

    /// Is replay complete?
    pub fn is_done(&self) -> bool {
        self.current_index >= self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::event::{EventPayload, OrderNewEvent, OrderAckEvent, OrderFillEvent};
    use atomic_core::types::*;

    fn make_events() -> Vec<Event> {
        let source = [0u8; 32];
        let sym = Symbol::new("BTC", "USDT", Venue::Simulated);
        let oid = OrderId::new(Venue::Simulated, 1);

        vec![
            Event::new(1, 1000, source, EventPayload::OrderNew(OrderNewEvent {
                order_id: oid.clone(), symbol: sym.clone(), side: Side::Buy,
                order_type: OrderType::Limit, price: Price(60000), qty: Qty(100),
                time_in_force: TimeInForce::GoodTilCancel, venue: Venue::Simulated,
            })),
            Event::new(2, 1001, source, EventPayload::OrderAck(OrderAckEvent {
                order_id: oid.clone(), exchange_order_id: "SIM-1".to_string(),
                venue: Venue::Simulated,
            })),
            Event::new(3, 1002, source, EventPayload::OrderFill(OrderFillEvent {
                order_id: oid.clone(), symbol: sym.clone(), side: Side::Buy,
                price: Price(60000), qty: Qty(50), remaining: Qty(50),
                fee: 3, is_maker: false, venue: Venue::Simulated,
            })),
            Event::new(4, 1003, source, EventPayload::OrderFill(OrderFillEvent {
                order_id: oid.clone(), symbol: sym.clone(), side: Side::Buy,
                price: Price(60000), qty: Qty(50), remaining: Qty(0),
                fee: 3, is_maker: false, venue: Venue::Simulated,
            })),
        ]
    }

    #[test]
    fn replay_determinism_same_events_same_hash() {
        let events = make_events();

        // Replay #1
        let mut player1 = ReplayPlayer::from_events(events.clone());
        let mut exec1 = atomic_execution::ExecutionEngine::new();
        while let Some(event) = player1.next() {
            let _ = exec1.process_event(event);
        }
        let hash1 = exec1.state_hash();

        // Replay #2 (same events)
        let mut player2 = ReplayPlayer::from_events(events);
        let mut exec2 = atomic_execution::ExecutionEngine::new();
        while let Some(event) = player2.next() {
            let _ = exec2.process_event(event);
        }
        let hash2 = exec2.state_hash();

        // Determinism guarantee: same events → same state hash
        assert_eq!(hash1, hash2, "Replay must be deterministic: same events must produce identical state hash");
    }

    #[test]
    fn replay_snapshot_restore_same_hash() {
        let events = make_events();

        // Full replay
        let mut player = ReplayPlayer::from_events(events.clone());
        let mut exec = atomic_execution::ExecutionEngine::new();
        while let Some(event) = player.next() {
            let _ = exec.process_event(event);
        }
        let original_hash = exec.state_hash();

        // Snapshot → restore → compare
        let snapshot = exec.snapshot(4, 1003);
        let mut exec2 = atomic_execution::ExecutionEngine::new();
        exec2.restore(&snapshot).unwrap();
        let restored_hash = exec2.state_hash();

        assert_eq!(original_hash, restored_hash, "Snapshot restore must produce identical state hash");
    }
}
