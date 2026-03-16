use thiserror::Error;

use crate::types::Venue;

#[derive(Debug, Error)]
pub enum AtomicError {
    // --- Sequencing ---
    #[error("sequence gap: expected {expected}, got {got}")]
    SequenceGap { expected: u64, got: u64 },

    #[error("duplicate sequence number: {0}")]
    DuplicateSeq(u64),

    // --- State ---
    #[error("state hash mismatch at seq {seq}: local={local_hash}, remote={remote_hash}")]
    StateHashMismatch {
        seq: u64,
        local_hash: String,
        remote_hash: String,
    },

    #[error("snapshot not found for seq {0}")]
    SnapshotNotFound(u64),

    // --- Execution ---
    #[error("order not found: {0}")]
    OrderNotFound(String),

    #[error("order already exists: {0}")]
    OrderAlreadyExists(String),

    #[error("invalid order state transition: {from} -> {to}")]
    InvalidOrderTransition { from: String, to: String },

    // --- Risk ---
    #[error("risk limit exceeded: {0}")]
    RiskLimitExceeded(String),

    #[error("kill switch activated: {0}")]
    KillSwitch(String),

    // --- Network ---
    #[error("node unreachable: {0}")]
    NodeUnreachable(String),

    #[error("consensus failed: {0}")]
    ConsensusFailed(String),

    #[error("connection to {venue} failed: {reason}")]
    ConnectionFailed { venue: Venue, reason: String },

    // --- Feed ---
    #[error("feed desync on {venue}: {reason}")]
    FeedDesync { venue: Venue, reason: String },

    #[error("unknown symbol: {0}")]
    UnknownSymbol(String),

    // --- Replay ---
    #[error("replay divergence at seq {seq}: {reason}")]
    ReplayDivergence { seq: u64, reason: String },

    // --- Serialization ---
    #[error("serialization error: {0}")]
    Serialization(String),

    // --- IO ---
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    // --- Config ---
    #[error("config error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, AtomicError>;
