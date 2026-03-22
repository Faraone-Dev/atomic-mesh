use atomic_core::event::Event;
use atomic_core::snapshot::EngineSnapshot;
use atomic_replay::EventLog;
use std::fs;
use std::path::Path;

/// Recovery plan computed on startup.
#[derive(Debug)]
pub struct RecoveryPlan {
    /// Latest snapshot found on disk (if any).
    pub snapshot: Option<EngineSnapshot>,
    /// Events from the log that need to be replayed after snapshot.
    pub events_to_replay: Vec<Event>,
    /// Whether we need to request missing events from peers.
    pub need_peer_sync: bool,
    /// If peer sync needed, request events from this seq.
    pub sync_from_seq: u64,
    /// Expected next seq (from event log).
    pub expected_next_seq: u64,
}

/// Recovery coordinator: handles snapshot persistence,
/// event log scanning, and recovery planning on startup.
pub struct RecoveryCoordinator {
    snapshot_dir: String,
    event_log_path: String,
}

impl RecoveryCoordinator {
    pub fn new(snapshot_dir: &str, event_log_path: &str) -> Self {
        Self {
            snapshot_dir: snapshot_dir.to_string(),
            event_log_path: event_log_path.to_string(),
        }
    }

    pub fn event_log_path(&self) -> &str {
        &self.event_log_path
    }

    /// Save a snapshot to disk (atomic: write temp + rename).
    pub fn save_snapshot(&self, snapshot: &EngineSnapshot) -> std::io::Result<()> {
        fs::create_dir_all(&self.snapshot_dir)?;
        let data = bincode::serialize(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        // Write numbered snapshot atomically: temp file + rename
        let filename = format!("{}/snapshot_{}.bin", self.snapshot_dir, snapshot.seq);
        let tmp_numbered = format!("{}.tmp", filename);
        fs::write(&tmp_numbered, &data)?;
        fs::rename(&tmp_numbered, &filename)?;

        // Write "latest" pointer atomically: temp file + rename
        let latest = format!("{}/snapshot_latest.bin", self.snapshot_dir);
        let tmp_latest = format!("{}/snapshot_latest.bin.tmp", self.snapshot_dir);
        fs::write(&tmp_latest, &data)?;
        fs::rename(&tmp_latest, &latest)?;

        tracing::info!("Snapshot saved: seq={} hash={}", snapshot.seq, hex::encode(snapshot.state_hash));
        Ok(())
    }

    /// Load the latest snapshot from disk.
    pub fn load_latest_snapshot(&self) -> std::io::Result<Option<EngineSnapshot>> {
        let latest = format!("{}/snapshot_latest.bin", self.snapshot_dir);
        if !Path::new(&latest).exists() {
            return Ok(None);
        }
        let data = fs::read(&latest)?;
        let snapshot: EngineSnapshot = bincode::deserialize(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        if !snapshot.verify() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot hash verification failed",
            ));
        }

        tracing::info!("Loaded snapshot: seq={} hash={}", snapshot.seq, hex::encode(snapshot.state_hash));
        Ok(Some(snapshot))
    }

    /// Scan the event log and build a recovery plan.
    pub fn plan_recovery(&self) -> std::io::Result<RecoveryPlan> {
        // Step 1: Try to load latest snapshot
        let snapshot = self.load_latest_snapshot()?;

        // Step 2: Read events from log
        let events = if Path::new(&self.event_log_path).exists() {
            let log = EventLog::read_only(&self.event_log_path);
            log.read_all()?
        } else {
            Vec::new()
        };

        // Step 3: Determine which events to replay
        let snapshot_seq = snapshot.as_ref().map(|s| s.seq).unwrap_or(0);
        let events_to_replay: Vec<Event> = events
            .into_iter()
            .filter(|e| e.seq > snapshot_seq)
            .collect();

        // Step 4: Detect gaps
        let mut need_peer_sync = false;
        let mut sync_from_seq = 0u64;
        let mut expected_next_seq = snapshot_seq + 1;

        if !events_to_replay.is_empty() {
            let first_seq = events_to_replay[0].seq;
            if first_seq > expected_next_seq {
                // Gap between snapshot and first event in log
                need_peer_sync = true;
                sync_from_seq = expected_next_seq;
                tracing::warn!(
                    "Event log gap detected: expected seq {}, found {}. Need peer sync.",
                    expected_next_seq, first_seq
                );
            }

            // Check for gaps within the log
            for window in events_to_replay.windows(2) {
                if window[1].seq != window[0].seq + 1 {
                    tracing::warn!(
                        "Event log internal gap: {} -> {}",
                        window[0].seq, window[1].seq
                    );
                    if !need_peer_sync {
                        need_peer_sync = true;
                        sync_from_seq = window[0].seq + 1;
                    }
                }
            }

            expected_next_seq = events_to_replay.last().unwrap().seq + 1;
        }

        tracing::info!(
            "Recovery plan: snapshot_seq={}, replay_events={}, need_sync={}, next_seq={}",
            snapshot_seq, events_to_replay.len(), need_peer_sync, expected_next_seq
        );

        Ok(RecoveryPlan {
            snapshot,
            events_to_replay,
            need_peer_sync,
            sync_from_seq,
            expected_next_seq,
        })
    }
}

/// Deduplicator for events received during recovery.
/// Prevents processing the same event twice.
pub struct EventDeduplicator {
    seen_seqs: std::collections::HashSet<u64>,
    max_seen: u64,
}

impl EventDeduplicator {
    pub fn new() -> Self {
        Self {
            seen_seqs: std::collections::HashSet::new(),
            max_seen: 0,
        }
    }

    pub fn from_seq(start_seq: u64) -> Self {
        Self {
            seen_seqs: std::collections::HashSet::new(),
            max_seen: start_seq,
        }
    }

    /// Returns true if this event has NOT been seen before (should be processed).
    pub fn check(&mut self, seq: u64) -> bool {
        if seq <= self.max_seen && !self.seen_seqs.insert(seq) {
            return false; // Duplicate
        }
        self.seen_seqs.insert(seq);
        if seq > self.max_seen {
            self.max_seen = seq;
        }
        // Prune old entries to prevent unbounded growth
        if self.seen_seqs.len() > 100_000 {
            let cutoff = self.max_seen.saturating_sub(50_000);
            self.seen_seqs.retain(|&s| s > cutoff);
        }
        true
    }

    pub fn max_seen(&self) -> u64 {
        self.max_seen
    }
}

impl Default for EventDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicator_rejects_duplicates() {
        let mut dedup = EventDeduplicator::new();
        assert!(dedup.check(1));
        assert!(dedup.check(2));
        assert!(dedup.check(3));
        assert!(!dedup.check(1)); // duplicate
        assert!(!dedup.check(2)); // duplicate
        assert!(dedup.check(4));
    }

    #[test]
    fn recovery_plan_cold_start() {
        // With no snapshot dir and no event log, should produce empty plan
        let coord = RecoveryCoordinator::new("/tmp/nonexistent_atomic_test", "/tmp/nonexistent.log");
        let plan = coord.plan_recovery().unwrap();
        assert!(plan.snapshot.is_none());
        assert!(plan.events_to_replay.is_empty());
        assert!(!plan.need_peer_sync);
        assert_eq!(plan.expected_next_seq, 1);
    }
}
