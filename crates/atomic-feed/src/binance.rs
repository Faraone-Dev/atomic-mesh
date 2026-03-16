use crate::connector::{FeedConnector, RawFeedMessage};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{error, info};

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/stream";
const BINANCE_TESTNET_WS_URL: &str = "wss://stream.testnet.binance.vision/stream";

/// Binance WebSocket feed connector.
pub struct BinanceConnector {
    rx: mpsc::Receiver<RawFeedMessage>,
    tx: mpsc::Sender<RawFeedMessage>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    testnet: bool,
}

impl BinanceConnector {
    pub fn new(buffer_size: usize, testnet: bool) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        Self { rx, tx, shutdown: None, testnet }
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
                if self.testnet {
                    // Testnet supports fewer stream types
                    vec![
                        format!("{}@depth20", lower),
                        format!("{}@trade", lower),
                    ]
                } else {
                    vec![
                        format!("{}@depth20@100ms", lower),
                        format!("{}@trade", lower),
                    ]
                }
            })
            .collect();

        let base = if self.testnet { BINANCE_TESTNET_WS_URL } else { BINANCE_WS_URL };
        // Use combined stream endpoint so messages include stream name wrapper
        let url = format!("{}?streams={}", base, streams.join("/"));
        info!("Binance WS URL: {}", url);
        info!("Testnet mode: {}", self.testnet);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.shutdown = Some(shutdown_tx);

        let tx = self.tx.clone();

        tokio::spawn(async move {
            let mut backoff_ms: u64 = 500;
            const MAX_BACKOFF_MS: u64 = 30_000;

            loop {
                let ws_result = connect_async(&url).await;
                let (_, mut read) = match ws_result {
                    Ok((ws, _)) => {
                        backoff_ms = 500; // reset on success
                        ((), ws)
                    }
                    Err(e) => {
                        error!("Binance WS connection failed: {} — URL: {} — retrying in {}ms", e, url, backoff_ms);
                        tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                        continue;
                    }
                };

                info!("Binance WS connected: {} streams", streams.len());

                let disconnected = loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                    if let Some(feed_msg) = parse_binance_message(&text) {
                                        if tx.send(feed_msg).await.is_err() {
                                            break true; // channel closed, stop entirely
                                        }
                                    }
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_data))) => {
                                    // Pong handled automatically by tungstenite
                                }
                                Some(Err(e)) => {
                                    error!("Binance WS error: {} — reconnecting in {}ms", e, backoff_ms);
                                    break false;
                                }
                                None => {
                                    error!("Binance WS closed — reconnecting in {}ms", backoff_ms);
                                    break false;
                                }
                                _ => {}
                            }
                        }
                        _ = &mut shutdown_rx => {
                            info!("Binance WS shutting down");
                            break true;
                        }
                    }
                };

                if disconnected {
                    break; // shutdown or channel closed
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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

fn symbol_from_stream(stream: &str) -> Option<String> {
    // "btcusdt@depth20" -> "BTCUSDT"
    stream.split('@').next().map(|s| s.to_uppercase())
}

fn parse_binance_message(text: &str) -> Option<RawFeedMessage> {
    // Try combined stream format first
    if let Ok(combined) = serde_json::from_str::<BinanceCombined>(text) {
        let stream = combined.stream.as_deref()?;
        let data = &combined.data;

        if stream.contains("@depth") {
            let sym = symbol_from_stream(stream)?;
            return parse_depth(data, &sym);
        } else if stream.contains("@trade") {
            return parse_trade(data);
        }
    }

    // Try direct format
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        // depth20 partial book snapshot: has "lastUpdateId" but no "e" field
        if value.get("lastUpdateId").is_some() && value.get("bids").is_some() {
            return parse_depth(&value, "UNKNOWN");
        }

        if let Some(event_type) = value.get("e").and_then(|v| v.as_str()) {
            match event_type {
                "depthUpdate" => {
                    let sym = value.get("s")?.as_str()?.to_string();
                    return parse_depth(&value, &sym);
                }
                "trade" => return parse_trade(&value),
                _ => {}
            }
        }
    }

    None
}

fn parse_depth(data: &serde_json::Value, symbol: &str) -> Option<RawFeedMessage> {
    // timestamp: use "E" if present (depthUpdate), otherwise use current time
    let timestamp = data.get("E").and_then(|v| v.as_u64()).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    });

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

    // depth20 is a full snapshot, depthUpdate is a delta
    let is_snapshot = data.get("lastUpdateId").is_some() && data.get("e").is_none();

    if is_snapshot {
        Some(RawFeedMessage::OrderBookSnapshot {
            symbol: symbol.to_string(),
            bids,
            asks,
            timestamp,
        })
    } else {
        Some(RawFeedMessage::OrderBookDelta {
            symbol: symbol.to_string(),
            bids,
            asks,
            timestamp,
        })
    }
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
