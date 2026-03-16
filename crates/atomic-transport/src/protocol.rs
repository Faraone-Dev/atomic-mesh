use atomic_core::event::Event;
use atomic_core::snapshot::EngineSnapshot;
use serde::{Deserialize, Serialize};

/// Wire protocol messages between mesh nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MeshMessage {
    /// Replicate an event to followers.
    EventReplication(Event),

    /// Batch of events for bulk replication.
    EventBatch(Vec<Event>),

    /// Heartbeat with current state.
    Heartbeat {
        node_id: [u8; 32],
        last_seq: u64,
        state_hash: [u8; 8],
        uptime_secs: u64,
    },

    /// Request events from a peer (catch-up).
    SyncRequest {
        from_seq: u64,
        to_seq: u64,
    },

    /// Response to sync request.
    SyncResponse {
        events: Vec<Event>,
    },

    /// Snapshot broadcast for state verification.
    SnapshotBroadcast(EngineSnapshot),

    /// State hash comparison (determinism check).
    HashVerify {
        seq: u64,
        hash: [u8; 8],
    },

    /// Vote for consensus (before order execution).
    ConsensusVote {
        proposal_seq: u64,
        node_id: [u8; 32],
        approved: bool,
    },

    /// Announcement when a node joins the mesh.
    NodeAnnounce {
        node_id: [u8; 32],
        role: atomic_core::node::NodeRole,
        addr: String,
    },
}

impl MeshMessage {
    /// Serialize to bytes for wire transmission.
    pub fn encode(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// Deserialize from bytes.
    pub fn decode(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }

    /// Message type name for logging.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::EventReplication(_) => "event_replication",
            Self::EventBatch(_) => "event_batch",
            Self::Heartbeat { .. } => "heartbeat",
            Self::SyncRequest { .. } => "sync_request",
            Self::SyncResponse { .. } => "sync_response",
            Self::SnapshotBroadcast(_) => "snapshot_broadcast",
            Self::HashVerify { .. } => "hash_verify",
            Self::ConsensusVote { .. } => "consensus_vote",
            Self::NodeAnnounce { .. } => "node_announce",
        }
    }
}
