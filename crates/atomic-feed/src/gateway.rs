use atomic_core::event::{
    Event, EventPayload, OrderAckEvent, OrderCancelEvent, OrderFillEvent, OrderRejectEvent,
};
use atomic_core::types::{OrderId, OrderType, Price, Qty, Side, Symbol, TimeInForce, Venue};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

/// Execution gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub api_key: String,
    pub api_secret: String,
    pub base_url: String,
    pub recv_window: u64,
    pub price_decimals: u8,
    pub qty_decimals: u8,
}

impl GatewayConfig {
    pub fn binance_spot(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
            base_url: "https://api.binance.com".to_string(),
            recv_window: 5000,
            price_decimals: 2,
            qty_decimals: 8,
        }
    }

    pub fn binance_testnet(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
            base_url: "https://testnet.binance.vision".to_string(),
            recv_window: 5000,
            price_decimals: 2,
            qty_decimals: 8,
        }
    }
}

/// Execution gateway: sends orders to Binance REST API and returns fill events.
pub struct ExecutionGateway {
    config: GatewayConfig,
    client: reqwest::Client,
    event_tx: mpsc::Sender<Event>,
    venue: Venue,
    source: [u8; 32],
}

/// Binance order response (new order).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct BinanceOrderResponse {
    symbol: String,
    order_id: u64,
    client_order_id: String,
    #[serde(default)]
    price: String,
    #[serde(default)]
    orig_qty: String,
    #[serde(default)]
    executed_qty: String,
    status: String,
    #[serde(default)]
    fills: Vec<BinanceFill>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct BinanceFill {
    price: String,
    qty: String,
    commission: String,
    #[serde(default)]
    commission_asset: String,
    #[serde(default)]
    is_maker: Option<bool>,
}

/// Binance cancel response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct BinanceCancelResponse {
    symbol: String,
    order_id: u64,
    client_order_id: String,
    status: String,
}

/// Binance error response.
#[derive(Debug, Deserialize)]
struct BinanceError {
    code: i32,
    msg: String,
}

impl ExecutionGateway {
    pub fn new(
        config: GatewayConfig,
        event_tx: mpsc::Sender<Event>,
        source: [u8; 32],
    ) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            event_tx,
            venue: Venue::Binance,
            source,
        }
    }

    /// Submit a new order to Binance.
    pub async fn submit_order(
        &self,
        order_id: &OrderId,
        symbol: &Symbol,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
        time_in_force: TimeInForce,
        seq_base: u64,
        timestamp: u64,
    ) {
        let binance_symbol = format!("{}{}", symbol.base, symbol.quote);

        let side_str = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };

        let type_str = match order_type {
            OrderType::Market => "MARKET",
            OrderType::Limit => "LIMIT",
            OrderType::StopMarket => "STOP_LOSS",
            OrderType::StopLimit => "STOP_LOSS_LIMIT",
        };

        let tif_str = match time_in_force {
            TimeInForce::GoodTilCancel => "GTC",
            TimeInForce::ImmediateOrCancel => "IOC",
            TimeInForce::FillOrKill => "FOK",
            _ => "GTC",
        };

        let price_f = price.to_f64(self.config.price_decimals);
        let qty_f = qty.to_f64(self.config.qty_decimals);

        let mut params: Vec<(&str, String)> = vec![
            ("symbol", binance_symbol),
            ("side", side_str.to_string()),
            ("type", type_str.to_string()),
            ("quantity", format!("{:.prec$}", qty_f, prec = self.config.qty_decimals as usize)),
            ("newClientOrderId", order_id.0.clone()),
            ("recvWindow", self.config.recv_window.to_string()),
        ];

        // Add price for limit orders
        if order_type == OrderType::Limit || order_type == OrderType::StopLimit {
            params.push(("price", format!("{:.prec$}", price_f, prec = self.config.price_decimals as usize)));
            params.push(("timeInForce", tif_str.to_string()));
        }

        // Add timestamp
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        params.push(("timestamp", ts_ms.to_string()));

        // Sign the request
        let query_string = build_query_string(&params);
        let signature = sign_hmac_sha256(&self.config.api_secret, &query_string);

        let url = format!(
            "{}/api/v3/order?{}&signature={}",
            self.config.base_url, query_string, signature
        );

        let result = self
            .client
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();

                if status.is_success() {
                    if let Ok(order_resp) = serde_json::from_str::<BinanceOrderResponse>(&body) {
                        self.handle_order_response(order_id, symbol, side, &order_resp, seq_base, timestamp).await;
                    } else {
                        warn!("Failed to parse Binance order response: {}", body);
                    }
                } else if let Ok(err) = serde_json::from_str::<BinanceError>(&body) {
                    self.send_reject(order_id, &format!("Binance {}: {}", err.code, err.msg), seq_base, timestamp).await;
                } else {
                    self.send_reject(order_id, &format!("HTTP {}: {}", status, body), seq_base, timestamp).await;
                }
            }
            Err(e) => {
                self.send_reject(order_id, &format!("network error: {}", e), seq_base, timestamp).await;
            }
        }
    }

    /// Cancel an order on Binance.
    pub async fn cancel_order(
        &self,
        order_id: &OrderId,
        symbol: &Symbol,
        seq_base: u64,
        timestamp: u64,
    ) {
        let binance_symbol = format!("{}{}", symbol.base, symbol.quote);

        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let params: Vec<(&str, String)> = vec![
            ("symbol", binance_symbol),
            ("origClientOrderId", order_id.0.clone()),
            ("recvWindow", self.config.recv_window.to_string()),
            ("timestamp", ts_ms.to_string()),
        ];

        let query_string = build_query_string(&params);
        let signature = sign_hmac_sha256(&self.config.api_secret, &query_string);

        let url = format!(
            "{}/api/v3/order?{}&signature={}",
            self.config.base_url, query_string, signature
        );

        let result = self
            .client
            .delete(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();

                if status.is_success() {
                    let event = Event::new(
                        seq_base,
                        timestamp,
                        self.source,
                        EventPayload::OrderCancel(OrderCancelEvent {
                            order_id: order_id.clone(),
                            reason: "requested".to_string(),
                            venue: self.venue,
                        }),
                    );
                    let _ = self.event_tx.send(event).await;
                    info!("Order {} canceled on Binance", order_id);
                } else {
                    warn!("Cancel failed for {}: {} {}", order_id, status, body);
                }
            }
            Err(e) => {
                error!("Cancel request failed for {}: {}", order_id, e);
            }
        }
    }

    /// Cancel ALL open orders for a symbol on Binance (DELETE /api/v3/openOrders).
    /// Call at startup to clear stale orders from previous sessions.
    pub async fn cancel_all_open_orders(&self, symbol_str: &str) -> u32 {
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let params: Vec<(&str, String)> = vec![
            ("symbol", symbol_str.to_string()),
            ("recvWindow", self.config.recv_window.to_string()),
            ("timestamp", ts_ms.to_string()),
        ];

        let query_string = build_query_string(&params);
        let signature = sign_hmac_sha256(&self.config.api_secret, &query_string);

        let url = format!(
            "{}/api/v3/openOrders?{}&signature={}",
            self.config.base_url, query_string, signature
        );

        let result = self
            .client
            .delete(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    // Response is an array of canceled orders
                    let count = serde_json::from_str::<Vec<serde_json::Value>>(&body)
                        .map(|v| v.len() as u32)
                        .unwrap_or(0);
                    info!("Startup: canceled {} stale open orders for {}", count, symbol_str);
                    count
                } else {
                    warn!("Startup cancel-all failed for {}: {} {}", symbol_str, status, body);
                    0
                }
            }
            Err(e) => {
                error!("Startup cancel-all request failed: {}", e);
                0
            }
        }
    }

    /// Start Binance user data stream to receive execution reports (fills).
    /// Creates a listenKey, opens a WebSocket, and spawns a background task
    /// that parses `executionReport` events into OrderFill/OrderCancel events.
    /// Returns the task handle so the caller can abort it on shutdown.
    pub async fn start_user_data_stream(&self) -> Option<tokio::task::JoinHandle<()>> {
        let api_key = self.config.api_key.clone();
        let _api_secret = self.config.api_secret.clone();
        let base_url = self.config.base_url.clone();
        let event_tx = self.event_tx.clone();
        let source = self.source;
        let venue = self.venue;
        let price_dec = self.config.price_decimals;
        let qty_dec = self.config.qty_decimals;
        let client = self.client.clone();

        // Create listenKey
        let listen_key = match create_listen_key(&client, &base_url, &api_key).await {
            Some(k) => k,
            None => {
                warn!("User data stream unavailable (testnet may not support it) — fills via REST only");
                return None;
            }
        };
        info!("User data stream listenKey created: {}...", &listen_key[..8.min(listen_key.len())]);

        // Determine WS base URL
        let ws_base = if base_url.contains("testnet") {
            "wss://stream.testnet.binance.vision"
        } else {
            "wss://stream.binance.com:9443"
        };
        let ws_url = format!("{}/ws/{}", ws_base, listen_key);

        let handle = tokio::spawn(async move {
            let mut backoff_ms: u64 = 1000;
            let mut seq_counter: u64 = 900_000_000; // high range to avoid collision with market data seq

            loop {
                let ws_result = connect_async(&ws_url).await;
                let (_, mut read) = match ws_result {
                    Ok((ws, _)) => {
                        backoff_ms = 1000;
                        info!("User data stream WS connected");
                        ((), ws)
                    }
                    Err(e) => {
                        error!("User data stream WS failed: {} — retrying in {}ms", e, backoff_ms);
                        tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                // Spawn keepalive task (PUT every 30 min)
                let keepalive_client = client.clone();
                let keepalive_url = base_url.clone();
                let keepalive_key = api_key.clone();
                let keepalive_lk = listen_key.clone();
                let keepalive_handle = tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(30 * 60)).await;
                        let url = format!(
                            "{}/api/v3/userDataStream?listenKey={}",
                            keepalive_url, keepalive_lk
                        );
                        let _ = keepalive_client
                            .put(&url)
                            .header("X-MBX-APIKEY", &keepalive_key)
                            .send()
                            .await;
                        info!("User data stream keepalive sent");
                    }
                });

                let disconnected = loop {
                    match read.next().await {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            if let Some(event) = parse_execution_report(
                                &text, source, venue, price_dec, qty_dec, &mut seq_counter,
                            ) {
                                if event_tx.send(event).await.is_err() {
                                    break true;
                                }
                            }
                        }
                        Some(Ok(_)) => {} // ping/pong/binary
                        Some(Err(e)) => {
                            error!("User data stream error: {} — reconnecting", e);
                            break false;
                        }
                        None => {
                            warn!("User data stream closed — reconnecting");
                            break false;
                        }
                    }
                };

                keepalive_handle.abort();

                if disconnected {
                    break;
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        });

        Some(handle)
    }

    async fn handle_order_response(
        &self,
        order_id: &OrderId,
        symbol: &Symbol,
        side: Side,
        resp: &BinanceOrderResponse,
        seq_base: u64,
        timestamp: u64,
    ) {
        // Send Ack
        let ack_event = Event::new(
            seq_base,
            timestamp,
            self.source,
            EventPayload::OrderAck(OrderAckEvent {
                order_id: order_id.clone(),
                exchange_order_id: resp.order_id.to_string(),
                venue: self.venue,
            }),
        );
        let _ = self.event_tx.send(ack_event).await;

        // Process immediate fills
        let mut fill_seq = seq_base + 1;
        let total_fills = resp.fills.len();

        for (i, fill) in resp.fills.iter().enumerate() {
            let fill_price = fill.price.parse::<f64>().unwrap_or(0.0);
            let fill_qty = fill.qty.parse::<f64>().unwrap_or(0.0);
            let commission = fill.commission.parse::<f64>().unwrap_or(0.0);

            let is_last = i == total_fills - 1;
            let exec_qty = resp.executed_qty.parse::<f64>().unwrap_or(0.0);
            let orig_qty = resp.orig_qty.parse::<f64>().unwrap_or(0.0);
            let remaining = orig_qty - exec_qty;

            let fill_data = OrderFillEvent {
                order_id: order_id.clone(),
                symbol: symbol.clone(),
                side,
                price: Price::from_f64(fill_price, self.config.price_decimals),
                qty: Qty::from_f64(fill_qty, self.config.qty_decimals),
                remaining: Qty::from_f64(remaining.max(0.0), self.config.qty_decimals),
                fee: (commission * 10f64.powi(self.config.price_decimals as i32)) as i64,
                is_maker: fill.is_maker.unwrap_or(false),
                venue: self.venue,
            };

            let payload = if is_last && remaining <= 0.0 {
                EventPayload::OrderFill(fill_data)
            } else {
                EventPayload::OrderPartialFill(fill_data)
            };

            let fill_event = Event::new(
                fill_seq,
                timestamp,
                self.source,
                payload,
            );
            let _ = self.event_tx.send(fill_event).await;
            fill_seq += 1;
        }

        info!(
            "Order {} acknowledged on Binance (exchange_id={}, fills={})",
            order_id, resp.order_id, total_fills
        );
    }

    async fn send_reject(&self, order_id: &OrderId, reason: &str, seq: u64, timestamp: u64) {
        let event = Event::new(
            seq,
            timestamp,
            self.source,
            EventPayload::OrderReject(OrderRejectEvent {
                order_id: order_id.clone(),
                reason: reason.to_string(),
                venue: self.venue,
            }),
        );
        let _ = self.event_tx.send(event).await;
        warn!("Order {} rejected: {}", order_id, reason);
    }
}

/// Build URL query string from params.
fn build_query_string(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

/// HMAC-SHA256 signature.
fn sign_hmac_sha256(secret: &str, message: &str) -> String {
    use ring::hmac;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let signature = hmac::sign(&key, message.as_bytes());
    hex::encode(signature.as_ref())
}

/// Create a Binance listenKey for user data stream.
async fn create_listen_key(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Option<String> {
    let url = format!("{}/api/v3/userDataStream", base_url);
    let resp = client
        .post(&url)
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!("listenKey request returned non-success: {}", body);
        return None;
    }

    #[derive(Deserialize)]
    struct ListenKeyResp {
        #[serde(rename = "listenKey")]
        listen_key: String,
    }
    let body = resp.text().await.ok()?;
    let parsed: ListenKeyResp = serde_json::from_str(&body).ok()?;
    Some(parsed.listen_key)
}

/// Parse a Binance executionReport from the user data stream into an Event.
/// Handles FILLED, PARTIALLY_FILLED, CANCELED, EXPIRED, and REJECTED statuses.
fn parse_execution_report(
    text: &str,
    source: [u8; 32],
    venue: Venue,
    price_dec: u8,
    qty_dec: u8,
    seq: &mut u64,
) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    let event_type = v.get("e")?.as_str()?;
    if event_type != "executionReport" {
        return None;
    }

    let client_order_id = v.get("c")?.as_str()?.to_string();
    let order_status = v.get("X")?.as_str()?;     // X = current order status
    let exec_type = v.get("x")?.as_str()?;        // x = execution type (TRADE, CANCELED, etc.)
    let side_str = v.get("S")?.as_str()?;
    let symbol_str = v.get("s")?.as_str()?;

    let side = match side_str {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        _ => return None,
    };

    // Build symbol (assume XXXUSDT format)
    let base = if symbol_str.ends_with("USDT") {
        &symbol_str[..symbol_str.len() - 4]
    } else {
        symbol_str
    };
    let quote = if symbol_str.ends_with("USDT") { "USDT" } else { "" };
    let symbol = Symbol::new(base, quote, venue);

    let order_id = OrderId(client_order_id);

    let ts_ms = v.get("E").and_then(|t| t.as_u64()).unwrap_or(0);
    let timestamp = ts_ms * 1_000_000; // ms → ns

    *seq += 1;

    match exec_type {
        "TRADE" => {
            // Fill event: "l" = last filled qty, "L" = last filled price, "n" = commission
            let fill_price: f64 = v.get("L")?.as_str()?.parse().ok()?;
            let fill_qty: f64 = v.get("l")?.as_str()?.parse().ok()?;
            let commission: f64 = v.get("n").and_then(|n| n.as_str()?.parse().ok()).unwrap_or(0.0);
            let remaining: f64 = v.get("q").and_then(|q| q.as_str()?.parse().ok()).unwrap_or(0.0)
                - v.get("z").and_then(|z| z.as_str()?.parse().ok()).unwrap_or(0.0);
            let is_maker = v.get("m").and_then(|m| m.as_bool()).unwrap_or(false);

            let fill_data = OrderFillEvent {
                order_id,
                symbol,
                side,
                price: Price::from_f64(fill_price, price_dec),
                qty: Qty::from_f64(fill_qty, qty_dec),
                remaining: Qty::from_f64(remaining.max(0.0), qty_dec),
                fee: (commission * 10f64.powi(price_dec as i32)) as i64,
                is_maker,
                venue,
            };

            let payload = if order_status == "FILLED" {
                EventPayload::OrderFill(fill_data)
            } else {
                EventPayload::OrderPartialFill(fill_data)
            };

            info!("USER STREAM FILL: {} {} @ {} (status={})",
                  side_str, fill_qty, fill_price, order_status);

            Some(Event::new(*seq, timestamp, source, payload))
        }
        "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" => {
            info!("USER STREAM {}: {}", exec_type, order_id);
            Some(Event::new(
                *seq,
                timestamp,
                source,
                EventPayload::OrderCancel(OrderCancelEvent {
                    order_id,
                    reason: exec_type.to_string(),
                    venue,
                }),
            ))
        }
        "REJECTED" => {
            let reason = v.get("r").and_then(|r| r.as_str()).unwrap_or("unknown");
            warn!("USER STREAM REJECT: {} reason={}", order_id, reason);
            Some(Event::new(
                *seq,
                timestamp,
                source,
                EventPayload::OrderReject(OrderRejectEvent {
                    order_id,
                    reason: reason.to_string(),
                    venue,
                }),
            ))
        }
        _ => None, // NEW, PENDING_CANCEL, etc. — ignore
    }
}
