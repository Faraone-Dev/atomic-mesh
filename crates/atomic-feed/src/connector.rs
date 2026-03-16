use atomic_core::types::Symbol;
use tokio::sync::mpsc;

/// Raw market data message from an exchange WebSocket.
#[derive(Debug, Clone)]
pub enum RawFeedMessage {
    OrderBookSnapshot {
        symbol: String,
        bids: Vec<(f64, f64)>,
        asks: Vec<(f64, f64)>,
        timestamp: u64,
    },
    OrderBookDelta {
        symbol: String,
        bids: Vec<(f64, f64)>,
        asks: Vec<(f64, f64)>,
        timestamp: u64,
    },
    Trade {
        symbol: String,
        price: f64,
        qty: f64,
        is_buy: bool,
        timestamp: u64,
    },
}

/// Trait for exchange WebSocket connectors.
#[async_trait::async_trait]
pub trait FeedConnector: Send + Sync {
    /// Connect to the exchange and subscribe to the given symbols.
    async fn connect(&mut self, symbols: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Disconnect from the exchange.
    async fn disconnect(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Get the message receiver channel.
    fn receiver(&mut self) -> &mut mpsc::Receiver<RawFeedMessage>;

    /// Exchange name for logging.
    fn name(&self) -> &str;
}
