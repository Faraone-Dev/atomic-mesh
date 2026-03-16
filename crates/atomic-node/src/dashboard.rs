use axum::{
    Router,
    extract::{State, ws::{Message, WebSocket, WebSocketUpgrade}},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

use atomic_core::metrics::PipelineMetrics;

/// Kill switch action requested from dashboard.
/// Main loop reads this via `DashboardState::pending_kill_action()`.
pub const KILL_NONE: u8 = 0;
pub const KILL_STOP_ALL: u8 = 1;
pub const KILL_CANCEL_ALL: u8 = 2;
pub const KILL_DISCONNECT: u8 = 3;

/// Shared state between main loop and dashboard.
pub struct DashboardState {
    pub metrics: &'static PipelineMetrics,
    pub event_tx: broadcast::Sender<DashboardEvent>,
    pub node_id: String,
    pub role: String,
    pub start_time: std::time::Instant,
    pub gateway_mode: String,
    pub peers: Vec<String>,
    pub symbols: Vec<String>,
    pub kill_action: AtomicU8,
}

impl DashboardState {
    /// Check and consume any pending kill action.
    pub fn pending_kill_action(&self) -> u8 {
        self.kill_action.swap(KILL_NONE, Ordering::SeqCst)
    }
}

/// Events pushed to the dashboard via WebSocket.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum DashboardEvent {
    #[serde(rename = "tick")]
    Tick {
        seq: u64,
        events: u64,
        orders: u64,
        fills: u64,
        rejects: u64,
        uptime_secs: u64,
        state_hash: String,
        msg_per_sec: f64,
        last_price: String,
        volume_session: String,
    },
    #[serde(rename = "order")]
    Order {
        id: String,
        symbol: String,
        side: String,
        order_type: String,
        price: String,
        qty: String,
        filled_qty: String,
        state: String,
        venue: String,
        latency_ns: u64,
    },
    #[serde(rename = "feed")]
    Feed {
        symbol: String,
        bid: String,
        ask: String,
        last_price: String,
        bid_qty: String,
        ask_qty: String,
    },
    #[serde(rename = "event_stream")]
    EventStream {
        seq: u64,
        timestamp: u64,
        event_type: String,
        symbol: String,
        detail: String,
    },
    #[serde(rename = "book_update")]
    BookUpdate {
        symbol: String,
        bids: Vec<[String; 2]>,
        asks: Vec<[String; 2]>,
        mid_price: String,
        spread: String,
        imbalance: String,
    },
    #[serde(rename = "node_status")]
    NodeStatus {
        node_id: String,
        role: String,
        status: String,
        last_seq: u64,
        state_hash: String,
        latency_ms: u64,
    },
    #[serde(rename = "strategy")]
    Strategy {
        id: String,
        symbol: String,
        enabled: bool,
        signals: u64,
        orders: u64,
        fills: u64,
        pnl: String,
    },
    #[serde(rename = "risk")]
    Risk {
        open_orders: u64,
        max_open: u64,
        daily_loss: String,
        max_daily_loss: String,
        exposure: String,
        max_exposure: String,
        drawdown_pct: String,
        max_drawdown_pct: String,
    },
    #[serde(rename = "pnl")]
    Pnl {
        total_realized: String,
        total_unrealized: String,
        total_pnl: String,
        drawdown: String,
        volume: String,
    },
    #[serde(rename = "hash_check")]
    HashCheck {
        node_id: String,
        seq: u64,
        hash: String,
        matches: bool,
    },
    #[serde(rename = "log")]
    Log {
        level: String,
        message: String,
        timestamp: u64,
    },
}

/// Start the dashboard HTTP + WebSocket server.
pub async fn start_dashboard(
    port: u16,
    state: Arc<DashboardState>,
) {
    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/api/status", get(api_status))
        .route("/api/metrics", get(api_metrics))
        .route("/api/kill", post(api_kill))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Dashboard failed to bind port {}: {} — skipping dashboard", port, e);
            return;
        }
    };
    info!("Dashboard running on http://localhost:{}", port);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("Dashboard server error: {}", e);
    }
}

async fn serve_dashboard() -> Html<&'static str> {
    Html(include_str!("../../../dashboard/index.html"))
}

#[derive(Serialize)]
struct StatusResponse {
    node_id: String,
    role: String,
    uptime_secs: u64,
    gateway_mode: String,
    total_events: u64,
    total_orders: u64,
    total_fills: u64,
    total_rejects: u64,
    peers: Vec<String>,
    symbols: Vec<String>,
}

async fn api_status(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let status = StatusResponse {
        node_id: state.node_id.clone(),
        role: state.role.clone(),
        uptime_secs: state.start_time.elapsed().as_secs(),
        gateway_mode: state.gateway_mode.clone(),
        total_events: state.metrics.total_events.load(Ordering::Relaxed),
        total_orders: state.metrics.total_orders.load(Ordering::Relaxed),
        total_fills: state.metrics.total_fills.load(Ordering::Relaxed),
        total_rejects: state.metrics.total_rejects.load(Ordering::Relaxed),
        peers: state.peers.clone(),
        symbols: state.symbols.clone(),
    };
    axum::Json(status)
}

#[derive(Serialize)]
struct LatencyInfo {
    name: String,
    count: u64,
    avg_ns: u64,
    min_ns: u64,
    max_ns: u64,
    p50_ns: u64,
    p99_ns: u64,
}

#[derive(Serialize)]
struct MetricsResponse {
    counters: StatusResponse,
    latencies: Vec<LatencyInfo>,
}

async fn api_metrics(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let names_and_snaps = [
        ("feed_recv", state.metrics.feed_recv.snapshot()),
        ("feed_normalize", state.metrics.feed_normalize.snapshot()),
        ("strategy_compute", state.metrics.strategy_compute.snapshot()),
        ("risk_check", state.metrics.risk_check.snapshot()),
        ("order_submit", state.metrics.order_submit.snapshot()),
        ("order_to_ack", state.metrics.order_to_ack.snapshot()),
        ("order_to_fill", state.metrics.order_to_fill.snapshot()),
        ("state_hash", state.metrics.state_hash.snapshot()),
    ];

    let latencies: Vec<LatencyInfo> = names_and_snaps
        .iter()
        .map(|(name, h)| LatencyInfo {
            name: name.to_string(),
            count: h.count,
            avg_ns: if h.count > 0 { h.sum_ns / h.count } else { 0 },
            min_ns: if h.min_ns == u64::MAX { 0 } else { h.min_ns },
            max_ns: h.max_ns,
            p50_ns: 0,
            p99_ns: 0,
        })
        .collect();

    let resp = MetricsResponse {
        counters: StatusResponse {
            node_id: state.node_id.clone(),
            role: state.role.clone(),
            uptime_secs: state.start_time.elapsed().as_secs(),
            gateway_mode: state.gateway_mode.clone(),
            total_events: state.metrics.total_events.load(Ordering::Relaxed),
            total_orders: state.metrics.total_orders.load(Ordering::Relaxed),
            total_fills: state.metrics.total_fills.load(Ordering::Relaxed),
            total_rejects: state.metrics.total_rejects.load(Ordering::Relaxed),
            peers: state.peers.clone(),
            symbols: state.symbols.clone(),
        },
        latencies,
    };
    axum::Json(resp)
}

#[derive(Deserialize)]
struct KillRequest {
    action: String,
}

async fn api_kill(
    State(state): State<Arc<DashboardState>>,
    axum::Json(body): axum::Json<KillRequest>,
) -> impl IntoResponse {
    let code = match body.action.as_str() {
        "stop_all" => KILL_STOP_ALL,
        "cancel_all" => KILL_CANCEL_ALL,
        "disconnect" => KILL_DISCONNECT,
        other => {
            warn!("Unknown kill action: {}", other);
            return axum::Json(serde_json::json!({ "ok": false, "error": "unknown action" }));
        }
    };
    state.kill_action.store(code, Ordering::SeqCst);
    info!("Kill switch activated: {}", body.action);

    // Broadcast a log event so all connected dashboards see it
    let _ = state.event_tx.send(DashboardEvent::Log {
        level: "warn".into(),
        message: format!("Kill switch: {}", body.action),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    });

    axum::Json(serde_json::json!({ "ok": true, "action": body.action }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<DashboardState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<DashboardState>) {
    let mut rx = state.event_tx.subscribe();

    let init = DashboardEvent::Tick {
        seq: 0,
        events: state.metrics.total_events.load(Ordering::Relaxed),
        orders: state.metrics.total_orders.load(Ordering::Relaxed),
        fills: state.metrics.total_fills.load(Ordering::Relaxed),
        rejects: state.metrics.total_rejects.load(Ordering::Relaxed),
        uptime_secs: state.start_time.elapsed().as_secs(),
        state_hash: String::new(),
        msg_per_sec: 0.0,
        last_price: "\u{2014}".to_string(),
        volume_session: "0.00".to_string(),
    };
    if let Ok(json) = serde_json::to_string(&init) {
        let _ = socket.send(Message::Text(json.into())).await;
    }

    loop {
        tokio::select! {
            Ok(event) = rx.recv() => {
                if let Ok(json) = serde_json::to_string(&event) {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}
