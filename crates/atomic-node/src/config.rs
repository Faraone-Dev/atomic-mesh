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
    #[serde(default)]
    pub gateway: GatewaySettings,
}

/// Exchange gateway settings.
/// API keys are loaded from environment variables for security.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySettings {
    /// "live" or "testnet"
    pub mode: String,
    /// Env var name for API key (default: ATOMIC_API_KEY)
    pub api_key_env: String,
    /// Env var name for API secret (default: ATOMIC_API_SECRET)
    pub api_secret_env: String,
    /// If true, orders are actually sent. If false, only logged (paper mode).
    pub enabled: bool,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            mode: "testnet".to_string(),
            api_key_env: "ATOMIC_API_KEY".to_string(),
            api_secret_env: "ATOMIC_API_SECRET".to_string(),
            enabled: false,
        }
    }
}

impl GatewaySettings {
    /// Reads API key from the environment variable specified in config.
    pub fn api_key(&self) -> Result<String, String> {
        std::env::var(&self.api_key_env)
            .map_err(|_| format!("env var '{}' not set", self.api_key_env))
    }

    /// Reads API secret from the environment variable specified in config.
    pub fn api_secret(&self) -> Result<String, String> {
        std::env::var(&self.api_secret_env)
            .map_err(|_| format!("env var '{}' not set", self.api_secret_env))
    }

    /// Returns true if mode is "testnet".
    pub fn is_testnet(&self) -> bool {
        self.mode == "testnet"
    }
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
            gateway: GatewaySettings::default(),
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
