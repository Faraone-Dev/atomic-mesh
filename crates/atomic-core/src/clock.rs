use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Lamport logical clock for deterministic event ordering.
/// Every node maintains its own Lamport clock.
/// On local event: tick() → counter + 1
/// On receive: witness(remote_ts) → max(local, remote) + 1
#[derive(Debug)]
pub struct LamportClock {
    counter: AtomicU64,
}

impl LamportClock {
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }

    pub fn from(initial: u64) -> Self {
        Self {
            counter: AtomicU64::new(initial),
        }
    }

    /// Increment and return new timestamp (local event).
    pub fn tick(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Witness a remote timestamp, advance past it.
    pub fn witness(&self, remote_ts: u64) -> u64 {
        loop {
            let current = self.counter.load(Ordering::SeqCst);
            let new_val = std::cmp::max(current, remote_ts) + 1;
            if self
                .counter
                .compare_exchange(current, new_val, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return new_val;
            }
        }
    }

    /// Current value without incrementing.
    pub fn now(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }

    /// Reset clock (for testing / replay).
    pub fn reset(&self) {
        self.counter.store(0, Ordering::SeqCst);
    }
}

impl Default for LamportClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Monotonic sequence generator for the event bus.
/// Guarantees strictly increasing seq numbers.
#[derive(Debug)]
pub struct SequenceGenerator {
    next: AtomicU64,
}

impl SequenceGenerator {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub fn from(start: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
        }
    }

    /// Get next sequence number (monotonically increasing).
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    /// Peek at the next value without advancing.
    pub fn peek(&self) -> u64 {
        self.next.load(Ordering::SeqCst)
    }
}

impl Default for SequenceGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic wall-clock wrapper.
/// In production: uses real monotonic nanos.
/// In replay/test: uses injected timestamps from the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClockMode {
    /// Real monotonic clock (production).
    Live,
    /// Replayed timestamps from event log (deterministic replay).
    Replay,
}

/// Wall-clock source that can be switched between live and replay.
#[derive(Debug)]
pub struct WallClock {
    mode: ClockMode,
    replay_time: AtomicU64,
}

impl WallClock {
    pub fn live() -> Self {
        Self {
            mode: ClockMode::Live,
            replay_time: AtomicU64::new(0),
        }
    }

    pub fn replay() -> Self {
        Self {
            mode: ClockMode::Replay,
            replay_time: AtomicU64::new(0),
        }
    }

    /// Get current time in nanoseconds.
    pub fn now_nanos(&self) -> u64 {
        match self.mode {
            ClockMode::Live => {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64
            }
            ClockMode::Replay => self.replay_time.load(Ordering::SeqCst),
        }
    }

    /// Advance replay clock (only valid in Replay mode).
    pub fn set_replay_time(&self, nanos: u64) {
        self.replay_time.store(nanos, Ordering::SeqCst);
    }
}
