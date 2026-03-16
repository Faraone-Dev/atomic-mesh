use atomic_core::node::NodeRole;
use atomic_core::types::Venue;
use atomic_risk::RiskLimits;
use serde::{Deserialize, Serialize};

/// Node configuration loaded from TOML/JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub node_id: String,
    pub role: NodeRole,
    pub bind_addr: String,
    pub peers: Vec<PeerConfig>,
    pub feeds: Vec<FeedConfig>,
    pub risk: RiskLimits,
    pub event_log_path: String,
    pub snapshot_interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub node_id: String,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedConfig {
    pub venue: Venue,
    pub symbols: Vec<String>,
    pub price_decimals: u8,
    pub qty_decimals: u8,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: "node-0".to_string(),
            role: NodeRole::Leader,
            bind_addr: "0.0.0.0:5001".to_string(),
            peers: Vec::new(),
            feeds: vec![FeedConfig {
                venue: Venue::Binance,
                symbols: vec!["BTCUSDT".to_string()],
                price_decimals: 2,
                qty_decimals: 8,
            }],
            risk: RiskLimits::default(),
            event_log_path: "data/events.log".to_string(),
            snapshot_interval: 10000,
        }
    }
}

impl NodeConfig {
    pub fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        if path.ends_with(".json") {
            Ok(serde_json::from_str(&content)?)
        } else {
            Err("unsupported config format (use .json)".into())
        }
    }
}
