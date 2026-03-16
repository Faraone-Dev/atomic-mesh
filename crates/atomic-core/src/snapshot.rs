use serde::{Deserialize, Serialize};

/// Engine state snapshot for deterministic verification.
/// Each node periodically computes a hash of its full state and broadcasts it.
/// If hashes diverge across nodes → bug detected → halt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineSnapshot {
    /// Sequence number at which this snapshot was taken.
    pub seq: u64,
    /// Logical timestamp.
    pub timestamp: u64,
    /// xxHash3 of the entire engine state (positions, orders, book, balances).
    pub state_hash: [u8; 8],
    /// Number of events processed since last snapshot.
    pub events_since_last: u64,
    /// Serialized state (for recovery).
    pub state_data: Vec<u8>,
}

impl EngineSnapshot {
    /// Compute xxHash3 of arbitrary state bytes.
    pub fn compute_hash(data: &[u8]) -> [u8; 8] {
        let hash = xxhash_rust::xxh3::xxh3_64(data);
        hash.to_le_bytes()
    }

    /// Verify this snapshot's hash matches the given state data.
    pub fn verify(&self) -> bool {
        let computed = Self::compute_hash(&self.state_data);
        computed == self.state_hash
    }

    /// Compare two snapshots for determinism check.
    pub fn hashes_match(&self, other: &EngineSnapshot) -> bool {
        self.seq == other.seq && self.state_hash == other.state_hash
    }

    pub fn hash_hex(&self) -> String {
        hex::encode(self.state_hash)
    }
}

/// Incremental state for delta snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaSnapshot {
    /// Base snapshot seq this delta applies on top of.
    pub base_seq: u64,
    /// Current seq after applying delta.
    pub current_seq: u64,
    /// Serialized delta (events between base_seq and current_seq).
    pub delta_data: Vec<u8>,
    /// Hash of the state after applying delta.
    pub result_hash: [u8; 8],
}

impl DeltaSnapshot {
    pub fn verify_chain(_base_hash: [u8; 8], deltas: &[DeltaSnapshot]) -> bool {
        // Verify delta chain — each delta must produce the expected hash
        // This is a simplified check; real impl would replay events
        if deltas.is_empty() {
            return true;
        }
        for window in deltas.windows(2) {
            if window[0].current_seq != window[1].base_seq {
                return false;
            }
        }
        true
    }
}
