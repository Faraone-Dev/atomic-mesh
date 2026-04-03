use atomic_core::node::NodeRole;
use atomic_core::types::Venue;
use atomic_risk::RiskLimits;
use serde::{Deserialize, Serialize};

/// Strategy (hot-path) parameters.
/// Controls the Avellaneda-Stoikov market maker and C++ hot-path engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Order size per quote in base-unit atoms (e.g. 100_000 = 0.001 BTC in satoshis).
    pub order_qty: u64,
    /// Maximum net inventory before skewing (in base-unit atoms).
    pub max_inventory: u64,
    /// Half-spread in pipettes (smallest price increment). E.g. 10 = $0.10 at 2-dec.
    pub half_spread_pipettes: i64,
    /// Gamma risk aversion parameter (×10^-4). E.g. 1000 = γ 0.1.
    pub gamma: i64,
    /// Ticks of data required before engine quotes.
    pub warmup_ticks: u32,
    /// Ticks after a quote before allowing another.
    pub cooldown_ticks: u32,
    /// Minimum mid-price move (pipettes) to trigger requote.
    pub requote_threshold: i64,
    /// Enable VPIN toxicity filter (disable on thin feeds like testnet).
    #[serde(default = "default_vpin_auto")]
    pub vpin_enabled: Option<bool>,
    /// Seconds after last exchange ack/fill before an open order is considered stale.
    #[serde(default = "default_stale_order_timeout_secs")]
    pub stale_order_timeout_secs: u64,
}

fn default_vpin_auto() -> Option<bool> { None }
fn default_stale_order_timeout_secs() -> u64 { 30 }

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            order_qty: 100_000,
            max_inventory: 20_000_000,
            half_spread_pipettes: 10,
            gamma: 1000,
            warmup_ticks: 3,
            cooldown_ticks: 2,
            requote_threshold: 5,
            vpin_enabled: None,
            stale_order_timeout_secs: 30,
        }
    }
}

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
    #[serde(default)]
    pub strategy: StrategyConfig,
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
            strategy: StrategyConfig::default(),
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
