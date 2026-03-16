use crate::connector::{FeedConnector, RawFeedMessage};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws";

/// Binance WebSocket feed connector.
pub struct BinanceConnector {
    rx: mpsc::Receiver<RawFeedMessage>,
    tx: mpsc::Sender<RawFeedMessage>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl BinanceConnector {
    pub fn new(buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        Self { rx, tx, shutdown: None }
    }
}

#[async_trait::async_trait]
impl FeedConnector for BinanceConnector {
    async fn connect(&mut self, symbols: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Build combined stream URL
        let streams: Vec<String> = symbols
            .iter()
            .flat_map(|s| {
                let lower = s.to_lowercase();
                vec![
                    format!("{}@depth20@100ms", lower),
                    format!("{}@trade", lower),
                ]
            })
            .collect();

        let url = format!("{}/{}", BINANCE_WS_URL, streams.join("/"));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.shutdown = Some(shutdown_tx);

        let tx = self.tx.clone();

        tokio::spawn(async move {
            let ws_result = connect_async(&url).await;
            let (_, mut read) = match ws_result {
                Ok((ws, _)) => ((), ws),
                Err(e) => {
                    error!("Binance WS connection failed: {}", e);
                    return;
                }
            };

            info!("Binance WS connected: {} streams", streams.len());

            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                if let Some(feed_msg) = parse_binance_message(&text) {
                                    if tx.send(feed_msg).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_data))) => {
                                // Pong handled automatically by tungstenite
                            }
                            Some(Err(e)) => {
                                error!("Binance WS error: {}", e);
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("Binance WS shutting down");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    fn receiver(&mut self) -> &mut mpsc::Receiver<RawFeedMessage> {
        &mut self.rx
    }

    fn name(&self) -> &str {
        "binance"
    }
}

// --- Binance JSON parsing ---

#[derive(Deserialize)]
struct BinanceDepth {
    #[serde(rename = "e")]
    event_type: Option<String>,
    #[serde(rename = "s")]
    symbol: Option<String>,
    #[serde(rename = "E")]
    event_time: Option<u64>,
    bids: Option<Vec<[serde_json::Value; 2]>>,
    asks: Option<Vec<[serde_json::Value; 2]>>,
}

#[derive(Deserialize)]
struct BinanceTrade {
    #[serde(rename = "e")]
    event_type: Option<String>,
    #[serde(rename = "s")]
    symbol: Option<String>,
    #[serde(rename = "E")]
    event_time: Option<u64>,
    #[serde(rename = "p")]
    price: Option<String>,
    #[serde(rename = "q")]
    qty: Option<String>,
    #[serde(rename = "m")]
    is_buyer_maker: Option<bool>,
}

#[derive(Deserialize)]
struct BinanceCombined {
    stream: Option<String>,
    data: serde_json::Value,
}

fn parse_binance_message(text: &str) -> Option<RawFeedMessage> {
    // Try combined stream format first
    if let Ok(combined) = serde_json::from_str::<BinanceCombined>(text) {
        let stream = combined.stream.as_deref()?;
        let data = &combined.data;

        if stream.contains("@depth") {
            return parse_depth(data);
        } else if stream.contains("@trade") {
            return parse_trade(data);
        }
    }

    // Try direct format
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let event_type = value.get("e")?.as_str()?;
        match event_type {
            "depthUpdate" => return parse_depth(&value),
            "trade" => return parse_trade(&value),
            _ => {}
        }
    }

    None
}

fn parse_depth(data: &serde_json::Value) -> Option<RawFeedMessage> {
    let symbol = data.get("s")?.as_str()?.to_string();
    let timestamp = data.get("E")?.as_u64()?;

    let parse_levels = |key: &str| -> Vec<(f64, f64)> {
        data.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|level| {
                        let price = level.get(0)?.as_str()?.parse::<f64>().ok()?;
                        let qty = level.get(1)?.as_str()?.parse::<f64>().ok()?;
                        Some((price, qty))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let bids = parse_levels("bids");
    let asks = parse_levels("asks");

    Some(RawFeedMessage::OrderBookDelta {
        symbol,
        bids,
        asks,
        timestamp,
    })
}

fn parse_trade(data: &serde_json::Value) -> Option<RawFeedMessage> {
    let symbol = data.get("s")?.as_str()?.to_string();
    let timestamp = data.get("E")?.as_u64()?;
    let price = data.get("p")?.as_str()?.parse::<f64>().ok()?;
    let qty = data.get("q")?.as_str()?.parse::<f64>().ok()?;
    let is_buyer_maker = data.get("m")?.as_bool()?;

    Some(RawFeedMessage::Trade {
        symbol,
        price,
        qty,
        is_buy: !is_buyer_maker,
        timestamp,
    })
}
