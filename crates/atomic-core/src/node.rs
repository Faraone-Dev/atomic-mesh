use serde::{Deserialize, Serialize};
use std::fmt;

/// 32-byte node identifier (derived from public key).
pub type NodeId = [u8; 32];

/// Role a node plays in the mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeRole {
    /// Assigns global sequence numbers, coordinates consensus.
    Leader,
    /// Replicates events, can be promoted to leader.
    Follower,
    /// Read-only, receives events but doesn't participate in consensus.
    Observer,
}

/// Current state of a node in the mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeState {
    /// Node is booting up, loading snapshot.
    Initializing,
    /// Node is catching up with the event log.
    Syncing,
    /// Node is fully operational.
    Ready,
    /// Node is processing events.
    Active,
    /// Node detected state divergence, halted.
    Diverged,
    /// Node is shutting down gracefully.
    Draining,
    /// Node is offline.
    Offline,
}

/// Information about a node in the mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: NodeId,
    pub role: NodeRole,
    pub state: NodeState,
    pub addr: String,
    pub last_seq: u64,
    pub last_heartbeat: u64,
    pub uptime_secs: u64,
}

impl NodeInfo {
    pub fn new(id: NodeId, role: NodeRole, addr: String) -> Self {
        Self {
            id,
            role,
            state: NodeState::Initializing,
            addr,
            last_seq: 0,
            last_heartbeat: 0,
            uptime_secs: 0,
        }
    }

    pub fn id_hex(&self) -> String {
        hex::encode(&self.id[..8])
    }
}

impl fmt::Display for NodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Node[{}] role={:?} state={:?} seq={}",
            self.id_hex(),
            self.role,
            self.state,
            self.last_seq
        )
    }
}
