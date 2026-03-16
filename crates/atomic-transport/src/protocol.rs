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

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::event::{Event, EventPayload, TradeEvent};
    use atomic_core::types::{Price, Qty, Side, Symbol, Venue};

    fn sample_event() -> Event {
        Event::new(
            42,
            1_000_000,
            [1u8; 32],
            EventPayload::Trade(TradeEvent {
                symbol: Symbol::new("BTC", "USDT", Venue::Binance),
                price: Price(5000000),
                qty: Qty(100000),
                side: Side::Buy,
                exchange_ts: 999,
            }),
        )
    }

    #[test]
    fn heartbeat_roundtrip() {
        let msg = MeshMessage::Heartbeat {
            node_id: [7u8; 32],
            last_seq: 100,
            state_hash: [0xAB; 8],
            uptime_secs: 3600,
        };
        let bytes = msg.encode().unwrap();
        let decoded = MeshMessage::decode(&bytes).unwrap();
        match decoded {
            MeshMessage::Heartbeat { node_id, last_seq, state_hash, uptime_secs } => {
                assert_eq!(node_id, [7u8; 32]);
                assert_eq!(last_seq, 100);
                assert_eq!(state_hash, [0xAB; 8]);
                assert_eq!(uptime_secs, 3600);
            }
            _ => panic!("expected Heartbeat"),
        }
    }

    #[test]
    fn event_replication_roundtrip() {
        let msg = MeshMessage::EventReplication(sample_event());
        let bytes = msg.encode().unwrap();
        let decoded = MeshMessage::decode(&bytes).unwrap();
        match decoded {
            MeshMessage::EventReplication(e) => {
                assert_eq!(e.seq, 42);
                assert_eq!(e.timestamp, 1_000_000);
            }
            _ => panic!("expected EventReplication"),
        }
    }

    #[test]
    fn sync_request_roundtrip() {
        let msg = MeshMessage::SyncRequest { from_seq: 10, to_seq: 50 };
        let bytes = msg.encode().unwrap();
        let decoded = MeshMessage::decode(&bytes).unwrap();
        match decoded {
            MeshMessage::SyncRequest { from_seq, to_seq } => {
                assert_eq!(from_seq, 10);
                assert_eq!(to_seq, 50);
            }
            _ => panic!("expected SyncRequest"),
        }
    }

    #[test]
    fn hash_verify_roundtrip() {
        let msg = MeshMessage::HashVerify { seq: 99, hash: [0xFF; 8] };
        let bytes = msg.encode().unwrap();
        let decoded = MeshMessage::decode(&bytes).unwrap();
        match decoded {
            MeshMessage::HashVerify { seq, hash } => {
                assert_eq!(seq, 99);
                assert_eq!(hash, [0xFF; 8]);
            }
            _ => panic!("expected HashVerify"),
        }
    }

    #[test]
    fn consensus_vote_roundtrip() {
        let msg = MeshMessage::ConsensusVote {
            proposal_seq: 42,
            node_id: [3u8; 32],
            approved: true,
        };
        let bytes = msg.encode().unwrap();
        let decoded = MeshMessage::decode(&bytes).unwrap();
        match decoded {
            MeshMessage::ConsensusVote { proposal_seq, node_id, approved } => {
                assert_eq!(proposal_seq, 42);
                assert_eq!(node_id, [3u8; 32]);
                assert!(approved);
            }
            _ => panic!("expected ConsensusVote"),
        }
    }

    #[test]
    fn event_batch_roundtrip() {
        let events = vec![sample_event(), sample_event()];
        let msg = MeshMessage::EventBatch(events);
        let bytes = msg.encode().unwrap();
        let decoded = MeshMessage::decode(&bytes).unwrap();
        match decoded {
            MeshMessage::EventBatch(batch) => assert_eq!(batch.len(), 2),
            _ => panic!("expected EventBatch"),
        }
    }

    #[test]
    fn type_name_returns_correct_string() {
        let msg = MeshMessage::Heartbeat {
            node_id: [0; 32], last_seq: 0, state_hash: [0; 8], uptime_secs: 0,
        };
        assert_eq!(msg.type_name(), "heartbeat");

        let msg2 = MeshMessage::SyncRequest { from_seq: 0, to_seq: 0 };
        assert_eq!(msg2.type_name(), "sync_request");
    }
}
