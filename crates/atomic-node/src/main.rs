use clap::Parser;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use tracing::{info, warn, error};

use atomic_core::clock::{LamportClock, SequenceGenerator, WallClock};
use atomic_core::event::{Event, EventPayload};
use atomic_core::metrics::PipelineMetrics;
use atomic_core::types::{OrderId, Symbol, Venue};
use atomic_bus::EventSequencer;
use atomic_execution::{ExecutionEngine, SimulatedExchange, SimulatorConfig, StateVerifier};
use atomic_feed::{FeedNormalizer, ExecutionGateway, GatewayConfig};
use atomic_feed::connector::FeedConnector;
use atomic_feed::binance::BinanceConnector;
use std::sync::Arc;
use atomic_orderbook::OrderBook;
use atomic_replay::EventLog;
use atomic_risk::RiskEngine;
use atomic_router::SmartOrderRouter;
use atomic_strategy::{StrategyEngine, MarketMaker};
use atomic_hotpath::HotPathEngine;
use atomic_transport::MeshTransport;
use atomic_transport::protocol::MeshMessage;
use atomic_node::{NodeConfig, RecoveryCoordinator, EventDeduplicator};
use atomic_node::dashboard::{
    DashboardState, DashboardEvent, start_dashboard,
    KILL_STOP_ALL, KILL_CANCEL_ALL, KILL_DISCONNECT,
};

#[derive(Parser, Debug)]
#[command(name = "atomic-node", about = "ATOMIC MESH — Distributed Deterministic Trading Engine")]
struct Cli {
    /// Path to node config file
    #[arg(short, long, default_value = "config.json")]
    config: String,

    /// Override node role
    #[arg(short, long)]
    role: Option<String>,

    /// Override bind address
    #[arg(short, long)]
    bind: Option<String>,

    /// Replay mode: replay events from log file
    #[arg(long)]
    replay: Option<String>,

    /// Backtest mode: replay with simulated exchange
    #[arg(long)]
    backtest: Option<String>,

    /// Generate default config and exit
    #[arg(long)]
    generate_config: bool,

    /// Dashboard HTTP port (default: 3000)
    #[arg(long, default_value = "3000")]
    dashboard_port: u16,

    /// Print metrics report at end of replay/backtest
    #[arg(long)]
    metrics: bool,

    /// Export backtest equity curve to CSV file
    #[arg(long)]
    equity_csv: Option<String>,
}

fn parse_venue_symbol(raw: &str, venue: Venue) -> Symbol {
    let upper = raw.to_uppercase();
    const QUOTES: [&str; 11] = [
        "USDT", "USDC", "BUSD", "FDUSD", "BTC", "ETH", "EUR", "USD", "TRY", "BNB", "JPY",
    ];

    for quote in QUOTES {
        if upper.ends_with(quote) && upper.len() > quote.len() {
            let base = &upper[..upper.len() - quote.len()];
            return Symbol::new(base, quote, venue);
        }
    }

    if upper.len() > 4 {
        let split = upper.len() - 4;
        return Symbol::new(&upper[..split], &upper[split..], venue);
    }
    if upper.len() > 3 {
        let split = upper.len() - 3;
        return Symbol::new(&upper[..split], &upper[split..], venue);
    }
    Symbol::new(&upper, "USDT", venue)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("atomic=debug,info"))
        .with_target(false)
        .with_thread_ids(true)
        .init();

    let cli = Cli::parse();

    if cli.generate_config {
        let config = NodeConfig::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        println!("{}", json);
        return;
    }

    // Load .env file if present (API keys, secrets)
    let dotenv_result = dotenvy::dotenv();
    info!("Working directory: {:?}", std::env::current_dir().unwrap_or_default());
    match &dotenv_result {
        Ok(path) => info!(".env loaded from: {:?}", path),
        Err(e) => warn!(".env not found: {} — will use system env vars", e),
    }
    let has_key = std::env::var("ATOMIC_API_KEY").is_ok();
    let has_secret = std::env::var("ATOMIC_API_SECRET").is_ok();
    info!("ATOMIC_API_KEY present: {}, ATOMIC_API_SECRET present: {}", has_key, has_secret);

    info!("╔══════════════════════════════════════╗");
    info!("║       ATOMIC MESH v0.1.0             ║");
    info!("║  Distributed Deterministic Engine    ║");
    info!("╚══════════════════════════════════════╝");

    // Load config
    let config = match NodeConfig::from_file(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load config from '{}': {}", cli.config, e);
            error!("Run with --generate-config to create a default config");
            std::process::exit(1);
        }
    };

    info!("Node ID: {}", config.node_id);
    info!("Role: {:?}", config.role);
    info!("Bind: {}", config.bind_addr);
    info!("Peers: {}", config.peers.len());
    info!("Feeds: {} venues", config.feeds.len());

    // Initialize metrics (static for dashboard sharing)
    let metrics: &'static PipelineMetrics = Box::leak(Box::new(PipelineMetrics::new()));

    // Initialize components
    let _clock = LamportClock::new();
    let wall_clock = WallClock::live();
    let _sequencer = EventSequencer::new(1_000_000);
    let mut execution = ExecutionEngine::new();
    let mut strategy_engine = StrategyEngine::new();
    let mut risk = RiskEngine::new(config.risk.clone());
    let mut verifier = StateVerifier::new(config.snapshot_interval);

    info!("All components initialized");
    info!("State verification interval: {} events", config.snapshot_interval);

    // === RECOVERY ===
    let snapshot_dir = format!("{}_snapshots", config.event_log_path);
    let recovery = RecoveryCoordinator::new(&snapshot_dir, &config.event_log_path);

    let plan = match recovery.plan_recovery() {
        Ok(p) => p,
        Err(e) => {
            warn!("Recovery scan failed (cold start): {}", e);
            atomic_node::RecoveryPlan {
                snapshot: None,
                events_to_replay: Vec::new(),
                need_peer_sync: false,
                sync_from_seq: 0,
                expected_next_seq: 1,
            }
        }
    };

    // Restore from snapshot if available
    if let Some(ref snapshot) = plan.snapshot {
        info!("Restoring from snapshot at seq={}", snapshot.seq);
        if let Err(e) = execution.restore(snapshot) {
            error!("Failed to restore from snapshot: {}", e);
            std::process::exit(1);
        }
        verifier.on_verified(snapshot.seq, snapshot.state_hash);
        info!("State restored successfully (hash={})", hex::encode(snapshot.state_hash));
    }

    // Replay events from log
    if !plan.events_to_replay.is_empty() {
        info!("Replaying {} events from log...", plan.events_to_replay.len());
        let mut dedup = EventDeduplicator::from_seq(plan.snapshot.as_ref().map(|s| s.seq).unwrap_or(0));

        for event in &plan.events_to_replay {
            if !dedup.check(event.seq) {
                continue;
            }
            let _ = execution.process_event(event);
            strategy_engine.process_event(event);
            metrics.total_events.fetch_add(1, Ordering::Relaxed);

            if verifier.tick() {
                let hash = execution.state_hash();
                verifier.on_verified(event.seq, hash);
            }
        }
        info!("Recovery replay complete. State hash: {}", hex::encode(execution.state_hash()));
    }

    if plan.need_peer_sync {
        warn!("Event log has gaps — peer sync needed from seq {} (will request after mesh connects)", plan.sync_from_seq);
    }

    // === REPLAY MODE ===
    if let Some(replay_path) = cli.replay {
        info!("REPLAY MODE: reading from {}", replay_path);
        run_replay(&replay_path, &mut execution, &mut strategy_engine, &mut verifier, &metrics);

        if cli.metrics {
            println!("\n{}", metrics.report());
        }
        return;
    }

    // === BACKTEST MODE ===
    if let Some(backtest_path) = cli.backtest {
        info!("BACKTEST MODE: reading from {}", backtest_path);
        run_backtest(&backtest_path, &mut execution, &mut verifier, &metrics, &config, cli.equity_csv.as_deref());

        if cli.metrics {
            println!("\n{}", metrics.report());
        }
        return;
    }

    // === LIVE MODE ===
    info!("Starting in LIVE mode...");

    // Create event log for persistence
    let event_log_path = &config.event_log_path;
    if let Some(parent) = std::path::Path::new(event_log_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut event_log = match EventLog::open(event_log_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to open event log '{}': {}", event_log_path, e);
            std::process::exit(1);
        }
    };
    info!("Event log: {}", event_log_path);

    // Setup Smart Order Router
    let mut router = SmartOrderRouter::new();

    // Register market maker strategy for each feed symbol
    for feed_cfg in &config.feeds {
        for sym_str in &feed_cfg.symbols {
            let symbol = atomic_core::types::Symbol::new(
                &sym_str[..sym_str.len().saturating_sub(4)],  // base
                &sym_str[sym_str.len().saturating_sub(4)..],  // quote
                feed_cfg.venue,
            );
            let strat_id = format!("mm_{}_{}", feed_cfg.venue, sym_str);
            let mm = MarketMaker::new(
                &strat_id,
                symbol,
                feed_cfg.venue,
                100_000,      // order_qty = 0.001 BTC (~$74)
                20_000_000,   // max_inventory = 0.2 BTC
                15,           // half_spread = 15 pipettes ($0.15)
                1000,         // gamma = 0.1 risk aversion (aggressive)
            );
            strategy_engine.register(Box::new(mm));
        }
    }
    strategy_engine.start_all();
    info!("Registered {} strategies", strategy_engine.strategy_count());

    // C++ hot-path engine: same parameters as Rust MM but runs in ~100ns
    let mut hotpath = HotPathEngine::new(
        100_000,       // order_qty = 0.001 BTC (~$74)
        20_000_000,    // max_inventory = 0.2 BTC
        10,            // half_spread = 10 pipettes ($0.10)
        1000,          // gamma = 0.1 (aggressive)
        3,             // warmup_ticks
        2,             // cooldown_ticks (faster requote)
        5,             // requote_threshold = $0.05 (requote on small moves)
        !config.gateway.is_testnet(), // vpin_enabled: on in live, off on testnet (too few trades)
    );
    info!("C++ hot-path engine initialized");

    let primary_symbol_obj = config
        .feeds
        .first()
        .and_then(|f| f.symbols.first().map(|s| parse_venue_symbol(s, f.venue)))
        .unwrap_or_else(|| parse_venue_symbol("BTCUSDT", Venue::Binance));
    let primary_symbol_code = format!("{}{}", primary_symbol_obj.base, primary_symbol_obj.quote);

    // Compute node ID bytes (used by gateway + mesh)
    let node_id_bytes = {
        let mut id = [0u8; 32];
        let bytes = config.node_id.as_bytes();
        let len = bytes.len().min(32);
        id[..len].copy_from_slice(&bytes[..len]);
        id
    };

    // Setup execution gateway
    let (gw_tx, mut gw_rx) = tokio::sync::mpsc::channel::<Event>(10_000);

    let gateway: Option<Arc<ExecutionGateway>> = if config.gateway.enabled {
        match (config.gateway.api_key(), config.gateway.api_secret()) {
            (Ok(key), Ok(secret)) => {
                let gw_cfg = if config.gateway.is_testnet() {
                    GatewayConfig::binance_testnet(key, secret)
                } else {
                    GatewayConfig::binance_spot(key, secret)
                };
                info!("Gateway ENABLED — mode: {}, url: {}", config.gateway.mode, gw_cfg.base_url);
                Some(Arc::new(ExecutionGateway::new(gw_cfg, gw_tx.clone(), node_id_bytes)))
            }
            (Err(e), _) | (_, Err(e)) => {
                error!("Gateway enabled but API keys missing: {}", e);
                error!("Set {} and {} in .env or environment", config.gateway.api_key_env, config.gateway.api_secret_env);
                std::process::exit(1);
            }
        }
    } else {
        info!("Gateway DISABLED — paper mode (orders logged only)");
        None
    };

    // Cancel stale open orders from previous sessions
    if let Some(ref gw) = gateway {
        for feed_cfg in &config.feeds {
            for sym_str in &feed_cfg.symbols {
                gw.cancel_all_open_orders(sym_str).await;
            }
        }
        // Start user data stream for fill notifications
        gw.start_user_data_stream().await;

        // Seed MM with initial inventory via market buy (testnet only)
        if config.gateway.is_testnet() {
            let seed_sym = primary_symbol_obj.clone();
            let seed_oid = OrderId("seed-buy-1".to_string());
            gw.submit_order(
                &seed_oid,
                &seed_sym,
                atomic_core::types::Side::Buy,
                atomic_core::types::OrderType::Market,
                atomic_core::types::Price(0), // price ignored for market orders
                atomic_core::types::Qty(100_000), // 0.001 BTC
                atomic_core::types::TimeInForce::GoodTilCancel,
                0,
                0,
            ).await;
            info!("Seed market buy sent for initial inventory (testnet)");
        } else {
            info!("Live mode — skipping seed market buy (manual inventory required)");
        }
    }

    // Setup QUIC mesh transport
    let bind_addr: std::net::SocketAddr = config.bind_addr.parse().unwrap_or_else(|_| {
        warn!("Invalid bind address '{}', using 0.0.0.0:5001", config.bind_addr);
        "0.0.0.0:5001".parse().unwrap()
    });
    let mut mesh = MeshTransport::new(node_id_bytes, bind_addr, 10_000);
    match mesh.start().await {
        Ok(()) => info!("QUIC mesh transport started on {}", bind_addr),
        Err(e) => warn!("Failed to start mesh transport: {} (continuing without mesh)", e),
    }

    // Connect to configured peers
    for peer in &config.peers {
        let mut peer_id = [0u8; 32];
        let bytes = peer.node_id.as_bytes();
        let len = bytes.len().min(32);
        peer_id[..len].copy_from_slice(&bytes[..len]);
        if let Ok(addr) = peer.addr.parse::<std::net::SocketAddr>() {
            match mesh.connect_to_peer(peer_id, addr).await {
                Ok(()) => info!("Connected to peer {} at {}", peer.node_id, peer.addr),
                Err(e) => warn!("Failed to connect to peer {}: {}", peer.node_id, e),
            }
        }
    }

    let mut mesh_rx = mesh.take_receiver();

    // Send SyncRequest if recovery detected gaps
    if plan.need_peer_sync {
        let sync_msg = MeshMessage::SyncRequest {
            from_seq: plan.sync_from_seq,
            to_seq: plan.expected_next_seq,
        };
        mesh.broadcast(&sync_msg).await;
        info!("SyncRequest broadcast to peers for seq {}..{}", plan.sync_from_seq, plan.expected_next_seq);
    }

    // Setup feed connectors
    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::channel::<Event>(100_000);
    let _seq_gen = SequenceGenerator::from(plan.expected_next_seq);

    for feed_cfg in &config.feeds {
        let mut connector = BinanceConnector::new(50_000, config.gateway.is_testnet());
        let symbols = feed_cfg.symbols.clone();
        let normalizer = FeedNormalizer::new(feed_cfg.venue, feed_cfg.price_decimals, feed_cfg.qty_decimals);
        let _tx = feed_tx.clone();
        let source = node_id_bytes;

        match connector.connect(&symbols).await {
            Ok(()) => info!("Feed connected: {} — {:?}", feed_cfg.venue, symbols),
            Err(e) => {
                error!("Failed to connect feed {}: {}", feed_cfg.venue, e);
                continue;
            }
        }

        // Spawn feed reader task
        let feed_tx2 = feed_tx.clone();
        tokio::spawn(async move {
            let mut conn = connector;
            let rx = conn.receiver();
            while let Some(raw_msg) = rx.recv().await {
                if let Some(payload) = normalizer.normalize(raw_msg) {
                    let event = Event::new(0, 0, source, payload);
                    if feed_tx2.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });
    }

    // Start dashboard
    let (dash_tx, _) = tokio::sync::broadcast::channel::<DashboardEvent>(10_000);
    let dash_state = Arc::new(DashboardState {
        metrics,
        event_tx: dash_tx.clone(),
        node_id: config.node_id.clone(),
        role: format!("{:?}", config.role),
        start_time: std::time::Instant::now(),
        gateway_mode: if !config.gateway.enabled {
            "paper".to_string()
        } else {
            config.gateway.mode.clone()
        },
        peers: config.peers.iter().map(|p| p.node_id.clone()).collect(),
        symbols: config.feeds.iter().flat_map(|f| f.symbols.clone()).collect(),
        kill_action: std::sync::atomic::AtomicU8::new(0),
    });
    let dash_state_main = Arc::clone(&dash_state);
    tokio::spawn(start_dashboard(cli.dashboard_port, dash_state));

    // Auto-open dashboard in browser
    let dash_url = format!("http://localhost:{}", cli.dashboard_port);
    info!("Opening dashboard: {}", dash_url);
    let _ = open::that(&dash_url);

    info!("Node is ready. Processing live events. Press Ctrl+C to shutdown.");

    // === MAIN EVENT LOOP ===
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let _ctrlc_handle = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        let _ = shutdown_tx.send(());
    });

    // Decimal precision for price/qty display and PnL normalization
    let price_decimals: u8 = config.feeds.first().map(|f| f.price_decimals).unwrap_or(2);
    let qty_decimals: u8 = config.feeds.first().map(|f| f.qty_decimals).unwrap_or(8);
    let qty_divisor = 10i64.pow(qty_decimals as u32);
    let price_divisor = 10f64.powi(price_decimals as i32);

    // Heartbeat ticker
    let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
    let mut last_seq = plan.expected_next_seq;
    let mut order_send_times: HashMap<String, std::time::Instant> = HashMap::new();
    let mut last_price_display = String::from("—");
    let mut last_mid_price_pipettes: i64 = 0;
    let mut msg_count: u64 = 0;
    let mut last_msg_count: u64 = 0;
    let mut last_rate_check = std::time::Instant::now();
    let mut events_per_sec: f64 = 0.0;
    let mut volume_session: f64 = 0.0;
    let mut last_reset_day: u64 = wall_clock.now_nanos() / 86_400_000_000_000; // UTC day number

    // Persistent order books keyed by symbol pair (e.g. "BTC/USDT")
    let mut books: HashMap<String, OrderBook> = HashMap::new();
    let mut last_book_emit = std::time::Instant::now();
    let book_emit_interval = std::time::Duration::from_millis(100); // throttle book updates to 10/s

    // Position tracker for accurate PnL: (signed_qty_pipettes, cost_basis_pipettes)
    // positive qty = long, negative = short
    let mut position_qty: i64 = 0;      // in raw qty units (satoshis)
    let mut cost_basis: i64 = 0;         // total cost in pipettes (price * qty / qty_divisor)
    let mut realized_pnl: i64 = 0;       // cumulative realized PnL in pipettes
    let mut last_flat_pnl: i64 = 0;      // PnL snapshot at last flat position (for round-trip tracking)

    loop {
        tokio::select! {
            // --- Feed events from exchanges ---
            Some(mut event) = feed_rx.recv() => {
                let _timer = atomic_core::metrics::StageTimer::start(&metrics.feed_recv);
                msg_count += 1;

                // Assign seq and timestamp
                last_seq += 1;
                event.seq = last_seq;
                event.timestamp = wall_clock.now_nanos();
                event.source = node_id_bytes;

                // Log event to disk (buffered, flush every 100 events)
                if let Err(e) = event_log.append(&event) {
                    error!("Failed to write event log: {}", e);
                } else if last_seq % 100 == 0 {
                    let _ = event_log.flush();
                }

                // Update order book if applicable
                if let EventPayload::OrderBookUpdate(ref obu) = event.payload {
                    let key = obu.symbol.pair();
                    let book = books.entry(key.clone()).or_insert_with(|| OrderBook::new(obu.symbol.clone()));
                    if obu.is_snapshot {
                        book.apply_snapshot(&obu.bids, &obu.asks, event.seq, event.timestamp);
                    } else {
                        book.apply_delta(&obu.bids, &obu.asks, event.seq, event.timestamp);
                    }
                    strategy_engine.update_book(key.clone(), book.clone());
                    router.update_book(&key, obu.symbol.venue, book.clone());

                    // Feed spread to risk engine for spread gate
                    if let Some(spread) = book.spread() {
                        risk.update_spread(spread);
                    }

                    // Always update live price for topbar
                    if let Some(mid) = book.mid_price() {
                        last_mid_price_pipettes = mid.0;
                        last_price_display = format!("{:.2}", mid.to_f64(price_decimals));
                    }

                    // --- Emit Feed + BookUpdate to dashboard (throttled to 10/s) ---
                    if last_book_emit.elapsed() >= book_emit_interval {
                        last_book_emit = std::time::Instant::now();
                        let bid = book.best_bid();
                        let ask = book.best_ask();
                        let _ = dash_tx.send(DashboardEvent::Feed {
                            symbol: format!("{}{}", obu.symbol.base, obu.symbol.quote),
                            bid: bid.as_ref().map_or("0".into(), |l| format!("{:.2}", l.price.to_f64(price_decimals))),
                            ask: ask.as_ref().map_or("0".into(), |l| format!("{:.2}", l.price.to_f64(price_decimals))),
                            last_price: book.mid_price().map_or("0".into(), |p| format!("{:.2}", p.to_f64(price_decimals))),
                            bid_qty: bid.as_ref().map_or("0".into(), |l| format!("{:.8}", l.qty.to_f64(qty_decimals))),
                            ask_qty: ask.as_ref().map_or("0".into(), |l| format!("{:.8}", l.qty.to_f64(qty_decimals))),
                        });

                        let top_bids = book.top_bids(15);
                        let top_asks = book.top_asks(15);
                        let bid_total: f64 = top_bids.iter().map(|l| l.qty.to_f64(qty_decimals)).sum();
                        let ask_total: f64 = top_asks.iter().map(|l| l.qty.to_f64(qty_decimals)).sum();
                        let imbalance = if bid_total + ask_total > 0.0 {
                            (bid_total - ask_total) / (bid_total + ask_total)
                        } else { 0.0 };
                        let _ = dash_tx.send(DashboardEvent::BookUpdate {
                            symbol: format!("{}{}", obu.symbol.base, obu.symbol.quote),
                            bids: top_bids.iter().map(|l| [format!("{:.2}", l.price.to_f64(price_decimals)), format!("{:.8}", l.qty.to_f64(qty_decimals))]).collect(),
                            asks: top_asks.iter().map(|l| [format!("{:.2}", l.price.to_f64(price_decimals)), format!("{:.8}", l.qty.to_f64(qty_decimals))]).collect(),
                            mid_price: book.mid_price().map_or("0".into(), |p| format!("{:.2}", p.to_f64(price_decimals))),
                            spread: book.spread().map_or("0".into(), |s| format!("{:.2}", s as f64 / price_divisor)),
                            imbalance: format!("{:.4}", imbalance),
                        });
                    }
                }

                // Process through execution engine (for replay determinism)
                let _ = execution.process_event(&event);

                // Apply any kill switch action requested via dashboard.
                match dash_state_main.pending_kill_action() {
                    KILL_STOP_ALL => {
                        risk.activate_kill_switch("manual STOP ALL from dashboard");
                        let open_ids = execution.open_order_ids();
                        if let Some(ref gw) = gateway {
                            let gw = Arc::clone(gw);
                            let sym = primary_symbol_code.clone();
                            tokio::spawn(async move {
                                gw.cancel_all_open_orders(&sym).await;
                            });
                        } else {
                            for _ in &open_ids {
                                risk.on_order_closed();
                            }
                        }
                        warn!("Dashboard kill action: STOP ALL executed (kill switch active)");
                    }
                    KILL_CANCEL_ALL => {
                        let open_ids = execution.open_order_ids();
                        if let Some(ref gw) = gateway {
                            let gw = Arc::clone(gw);
                            let sym = primary_symbol_code.clone();
                            tokio::spawn(async move {
                                gw.cancel_all_open_orders(&sym).await;
                            });
                        } else {
                            for _ in &open_ids {
                                risk.on_order_closed();
                            }
                        }
                        warn!("Dashboard kill action: CANCEL ALL executed");
                    }
                    KILL_DISCONNECT => {
                        let open_ids = execution.open_order_ids();
                        if let Some(ref gw) = gateway {
                            let gw = Arc::clone(gw);
                            let sym = primary_symbol_code.clone();
                            tokio::spawn(async move {
                                gw.cancel_all_open_orders(&sym).await;
                            });
                        } else {
                            for _ in &open_ids {
                                risk.on_order_closed();
                            }
                        }
                        warn!("Dashboard kill action: DISCONNECT requested (gateway sessions cannot be force-closed yet, orders canceled)");
                    }
                    _ => {}
                }

                // ── C++ HOT PATH ─────────────────────────────────────
                // Route event through the C++ engine (~100-500ns) instead
                // of the Rust strategy engine for production commands.
                let hp_result = match &event.payload {
                    EventPayload::OrderBookUpdate(obu) => {
                        hotpath.on_book_update(&obu.bids, &obu.asks, obu.is_snapshot)
                    }
                    EventPayload::Trade(t) => {
                        hotpath.on_trade(t.side, t.price, t.qty)
                    }
                    EventPayload::OrderFill(fill) => {
                        hotpath.on_fill(fill.side, fill.qty, fill.price);
                        atomic_hotpath::HpResult::default()
                    }
                    _ => atomic_hotpath::HpResult::default(),
                };

                // Keep Rust strategy engine in sync (state tracking only — no commands executed)
                let _stimer = atomic_core::metrics::StageTimer::start(&metrics.strategy_compute);
                strategy_engine.process_event(&event);
                drop(_stimer);

                // Convert C++ hot-path commands to StrategyCommands
                let cmd_symbol = match &event.payload {
                    EventPayload::OrderBookUpdate(obu) => obu.symbol.clone(),
                    EventPayload::Trade(t) => t.symbol.clone(),
                    EventPayload::OrderFill(fill) | EventPayload::OrderPartialFill(fill) => fill.symbol.clone(),
                    _ => primary_symbol_obj.clone(),
                };
                let mut commands = Vec::new();
                for i in 0..hp_result.count {
                    let cmd = &hp_result.commands[i as usize];
                    match cmd.tag {
                        1 => { // HP_CMD_PLACE_BID
                            commands.push(atomic_strategy::StrategyCommand::PlaceOrder {
                                symbol: cmd_symbol.clone(),
                                side: atomic_core::types::Side::Buy,
                                order_type: atomic_core::types::OrderType::Limit,
                                price: atomic_core::types::Price(cmd.price),
                                qty: atomic_core::types::Qty(cmd.qty),
                                time_in_force: atomic_core::types::TimeInForce::GoodTilCancel,
                                venue: cmd_symbol.venue,
                            });
                        }
                        2 => { // HP_CMD_PLACE_ASK
                            commands.push(atomic_strategy::StrategyCommand::PlaceOrder {
                                symbol: cmd_symbol.clone(),
                                side: atomic_core::types::Side::Sell,
                                order_type: atomic_core::types::OrderType::Limit,
                                price: atomic_core::types::Price(cmd.price),
                                qty: atomic_core::types::Qty(cmd.qty),
                                time_in_force: atomic_core::types::TimeInForce::GoodTilCancel,
                                venue: cmd_symbol.venue,
                            });
                        }
                        3 => { // HP_CMD_CANCEL_ALL
                            commands.push(atomic_strategy::StrategyCommand::CancelAll {
                                symbol: Some(cmd_symbol.clone()),
                            });
                        }
                        _ => {}
                    }
                }

                if !commands.is_empty() {
                    info!("C++ HP generated {} commands in {}ns (fv={} vpin={} skew={} hs={})",
                        commands.len(), hp_result.compute_ns,
                        hp_result.fair_value, hp_result.vpin,
                        hp_result.skew, hp_result.half_spread);
                }

                // Execute strategy commands
                for cmd in commands {
                    match cmd {
                        atomic_strategy::StrategyCommand::PlaceOrder {
                            symbol, side, order_type, price, qty, time_in_force, venue,
                        } => {
                            let venue_for_position = symbol.venue;
                            let pos_symbol = symbol.clone();
                            let maybe_position = if position_qty != 0 {
                                let side = if position_qty > 0 {
                                    atomic_core::types::Side::Buy
                                } else {
                                    atomic_core::types::Side::Sell
                                };
                                Some(atomic_core::types::Position {
                                    symbol: pos_symbol,
                                    side,
                                    qty: atomic_core::types::Qty(position_qty.unsigned_abs()),
                                    avg_entry: atomic_core::types::Price(if position_qty == 0 { 0 } else { cost_basis / position_qty }),
                                    unrealized_pnl: 0,
                                    realized_pnl,
                                })
                            } else {
                                None
                            };

                            // Risk check
                            let _rtimer = atomic_core::metrics::StageTimer::start(&metrics.risk_check);
                            let risk_result = risk.check_order(
                                &symbol,
                                side,
                                qty,
                                price,
                                maybe_position.as_ref(),
                                event.timestamp,
                            );
                            drop(_rtimer);

                            if let Err(e) = risk_result {
                                warn!("Risk rejected order: {}", e);
                                metrics.total_rejects.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }

                            // Route order
                            let order_id = execution.next_order_id(venue);

                            // Register in execution engine
                            let live_order = atomic_execution::LiveOrder::new(
                                order_id.clone(),
                                symbol.clone(),
                                side,
                                order_type,
                                price,
                                qty,
                                time_in_force,
                                venue,
                                event.timestamp,
                            );
                            let _ = execution.register_order(live_order);
                            risk.on_order_opened();
                            metrics.total_orders.fetch_add(1, Ordering::Relaxed);

                            // Log the order event
                            last_seq += 1;
                            let order_event = Event::new(
                                last_seq,
                                wall_clock.now_nanos(),
                                node_id_bytes,
                                EventPayload::OrderNew(atomic_core::event::OrderNewEvent {
                                    order_id: order_id.clone(),
                                    symbol: symbol.clone(),
                                    side,
                                    order_type,
                                    price,
                                    qty,
                                    time_in_force,
                                    venue,
                                }),
                            );
                            let _ = event_log.append(&order_event);

                            // Submit to exchange
                            if let Some(ref gw) = gateway {
                                let gw = Arc::clone(gw);
                                let oid = order_id.clone();
                                let sym = symbol.clone();
                                let seq = last_seq;
                                let ts = wall_clock.now_nanos();
                                tokio::spawn(async move {
                                    gw.submit_order(&oid, &sym, side, order_type, price, qty, time_in_force, seq, ts).await;
                                });
                                order_send_times.insert(order_id.0.clone(), std::time::Instant::now());
                                info!("ORDER SENT: {:?} {:.8} {} @ {:.2} on {}", side, qty.to_f64(qty_decimals), symbol, price.to_f64(price_decimals), venue);
                            } else {
                                info!("ORDER PAPER: {:?} {:.8} {} @ {:.2} on {}", side, qty.to_f64(qty_decimals), symbol, price.to_f64(price_decimals), venue);
                            }

                            // Push order to dashboard
                            let _ = dash_tx.send(DashboardEvent::Order {
                                id: order_id.0.clone(),
                                symbol: format!("{}{}", symbol.base, symbol.quote),
                                side: format!("{:?}", side),
                                order_type: "Limit".to_string(),
                                price: format!("{:.2}", price.to_f64(price_decimals)),
                                qty: format!("{:.8}", qty.to_f64(qty_decimals)),
                                filled_qty: "0".to_string(),
                                state: "New".to_string(),
                                venue: format!("{}", venue_for_position),
                                latency_ns: 0,
                            });

                            // Push risk state to dashboard
                            let exposure_pipettes = (position_qty.unsigned_abs() as i64)
                                .saturating_mul(last_mid_price_pipettes)
                                / qty_divisor;
                            let drawdown_pct = if risk.peak_pnl() > 0 {
                                (risk.peak_pnl() - risk.total_pnl()) as f64 / risk.peak_pnl() as f64 * 100.0
                            } else {
                                0.0
                            };
                            let _ = dash_tx.send(DashboardEvent::Risk {
                                open_orders: risk.open_order_count() as u64,
                                max_open: config.risk.max_open_orders as u64,
                                daily_loss: format!("{:.2}", risk.total_pnl() as f64 / price_divisor),
                                max_daily_loss: format!("{:.2}", config.risk.max_loss as f64 / price_divisor),
                                exposure: format!("{:.2}", exposure_pipettes as f64 / price_divisor),
                                max_exposure: format!("{:.2}", config.risk.max_notional as f64 / price_divisor),
                                drawdown_pct: format!("{:.2}", drawdown_pct),
                                max_drawdown_pct: format!("{:.2}", config.risk.max_drawdown_bps as f64 / 100.0),
                            });
                        }
                        atomic_strategy::StrategyCommand::CancelOrder { order_id } => {
                            if let Some(ref gw) = gateway {
                                let gw = Arc::clone(gw);
                                let oid = order_id.clone();
                                let sym = execution
                                    .get_order(&oid.0)
                                    .map(|o| o.symbol.clone())
                                    .unwrap_or_else(|| primary_symbol_obj.clone());
                                let seq = last_seq;
                                let ts = wall_clock.now_nanos();
                                tokio::spawn(async move {
                                    gw.cancel_order(&oid, &sym, seq, ts).await;
                                });
                                info!("CANCEL SENT: {}", order_id);
                            } else {
                                info!("CANCEL PAPER: {}", order_id);
                            }
                        }
                        atomic_strategy::StrategyCommand::CancelAll { symbol } => {
                            let open_ids = execution.open_order_ids();
                            if let Some(ref gw) = gateway {
                                let gw = Arc::clone(gw);
                                let sym_str = symbol.as_ref()
                                    .map(|s| format!("{}{}", s.base, s.quote))
                                    .unwrap_or_else(|| primary_symbol_code.clone());
                                tokio::spawn(async move {
                                    gw.cancel_all_open_orders(&sym_str).await;
                                });
                            } else {
                                for _ in &open_ids {
                                    risk.on_order_closed();
                                }
                            }
                            info!("CANCEL ALL: {:?}", symbol);
                        }
                    }
                }

                metrics.total_events.fetch_add(1, Ordering::Relaxed);

                // --- Emit EventStream to dashboard ---
                let evt_type = match &event.payload {
                    EventPayload::OrderBookUpdate(_) => "book",
                    EventPayload::Trade(_) => "trade",
                    EventPayload::OrderNew(_) => "order_new",
                    EventPayload::OrderFill(_) | EventPayload::OrderPartialFill(_) => "fill",
                    EventPayload::OrderReject(_) => "reject",
                    EventPayload::OrderCancel(_) => "cancel",
                    _ => "other",
                };
                let _ = dash_tx.send(DashboardEvent::EventStream {
                    seq: event.seq,
                    timestamp: event.timestamp,
                    event_type: evt_type.to_string(),
                    symbol: String::new(),
                    detail: format!("{:?}", evt_type),
                });

                // Periodic state verification
                if verifier.tick() {
                    let _htimer = atomic_core::metrics::StageTimer::start(&metrics.state_hash);
                    let hash = execution.state_hash();
                    verifier.on_verified(event.seq, hash);
                    drop(_htimer);

                    // Broadcast hash to mesh peers
                    let hash_msg = MeshMessage::HashVerify {
                        seq: event.seq,
                        hash,
                    };
                    mesh.broadcast(&hash_msg).await;
                }

                // Periodic flush
                if last_seq % 1000 == 0 {
                    let _ = event_log.flush();
                }
            }

            // --- Gateway events (acks, fills, rejects from exchange) ---
            Some(gw_event) = gw_rx.recv() => {
                let _ = execution.process_event(&gw_event);
                let _ = event_log.append(&gw_event);
                strategy_engine.process_event(&gw_event);

                // Feed fills into C++ hot-path engine for inventory tracking
                if let EventPayload::OrderFill(ref fill) | EventPayload::OrderPartialFill(ref fill) = &gw_event.payload {
                    hotpath.on_fill(fill.side, fill.qty, fill.price);
                }

                match &gw_event.payload {
                    EventPayload::OrderFill(fill) | EventPayload::OrderPartialFill(fill) => {
                        metrics.total_fills.fetch_add(1, Ordering::Relaxed);
                        risk.on_order_closed();

                        // Fill latency
                        let fill_latency_ns = order_send_times.remove(&fill.order_id.0)
                            .map(|t| t.elapsed().as_nanos() as u64)
                            .unwrap_or(0);
                        if fill_latency_ns > 0 {
                            metrics.order_to_fill.record(fill_latency_ns);
                        }

                        // --- PnL computation from fill ---
                        // price is in pipettes (10^price_dec), qty in base units (10^qty_dec)
                        // fill_value = price * qty / 10^qty_dec → result in pipettes (quote currency)
                        let fill_value = (fill.price.0 as i64) * (fill.qty.0 as i64) / qty_divisor;
                        let fill_qty = fill.qty.0 as i64;
                        let fee_cost = fill.fee;

                        // Track position and compute realized PnL using average cost basis
                        match fill.side {
                            atomic_core::types::Side::Buy => {
                                if position_qty >= 0 {
                                    // Adding to long (or opening long from flat): just add cost
                                    position_qty += fill_qty;
                                    cost_basis += fill_value;
                                } else {
                                    // Closing short: realize PnL on min(fill_qty, abs(position))
                                    let close_qty = fill_qty.min(-position_qty);
                                    // Average entry price of the short (in pipettes per unit)
                                    let avg_entry = cost_basis as f64 / position_qty as f64; // negative / negative = positive
                                    let close_value = (avg_entry * close_qty as f64) as i64;
                                    // Short PnL: sold high, bought low → profit = entry_value - close_cost
                                    realized_pnl += close_value - (fill.price.0 as i64) * close_qty / qty_divisor;
                                    position_qty += fill_qty;
                                    cost_basis += (fill.price.0 as i64) * close_qty / qty_divisor; // reduce cost basis for closed portion
                                    let open_qty = fill_qty - close_qty;
                                    if open_qty > 0 {
                                        // Flipped to long
                                        cost_basis = (fill.price.0 as i64) * open_qty / qty_divisor;
                                    }
                                }
                            }
                            atomic_core::types::Side::Sell => {
                                if position_qty <= 0 {
                                    // Adding to short (or opening short from flat): add cost (negative side)
                                    position_qty -= fill_qty;
                                    cost_basis -= fill_value;  // negative cost = short entry value
                                } else {
                                    // Closing long: realize PnL on min(fill_qty, position)
                                    let close_qty = fill_qty.min(position_qty);
                                    // Average entry price of the long (in pipettes)
                                    let avg_entry = cost_basis as f64 / position_qty as f64;
                                    let entry_value = (avg_entry * close_qty as f64) as i64;
                                    // Long PnL: bought low, sold high → profit = sell_value - entry_value
                                    realized_pnl += (fill.price.0 as i64) * close_qty / qty_divisor - entry_value;
                                    position_qty -= fill_qty;
                                    cost_basis -= entry_value; // remove closed portion from cost basis
                                    let open_qty = fill_qty - close_qty;
                                    if open_qty > 0 {
                                        // Flipped to short
                                        cost_basis = -((fill.price.0 as i64) * open_qty / qty_divisor);
                                    }
                                }
                            }
                        }
                        // Subtract fees from realized PnL
                        realized_pnl -= fee_cost;
                        risk.update_pnl(realized_pnl - risk.total_pnl());

                        // Round-trip tracking for consecutive loss circuit breaker
                        if position_qty == 0 {
                            let trade_pnl = realized_pnl - last_flat_pnl;
                            if trade_pnl >= 0 {
                                risk.record_win();
                            } else {
                                risk.record_loss();
                            }
                            last_flat_pnl = realized_pnl;
                        }

                        // Convert to human-readable dollars for dashboard
                        let pnl_usd = risk.total_pnl() as f64 / price_divisor;
                        let vol_usd = fill_value as f64 / price_divisor;
                        volume_session += vol_usd.abs();

                        info!("💰 FILL: {} {} {:.8} @ {:.2} | value=${:.2} | PnL=${:.4} | pos={} | vol=${:.2}",
                              fill.order_id,
                              if fill.side == atomic_core::types::Side::Buy { "BUY" } else { "SELL" },
                              fill.qty.to_f64(8),
                              fill.price.to_f64(2),
                              vol_usd,
                              pnl_usd,
                              position_qty,
                              volume_session,
                        );

                        // Auto-reseed: when inventory drops to 0, market buy to refill (testnet only)
                        if position_qty == 0 && config.gateway.is_testnet() {
                            if let Some(ref gw) = gateway {
                                let gw = Arc::clone(gw);
                                let reseed_id = OrderId(format!("reseed-{}", last_seq));
                                let sym = primary_symbol_obj.clone();
                                info!("🔄 RESEED: inventory=0, sending market buy (testnet)");
                                tokio::spawn(async move {
                                    gw.submit_order(
                                        &reseed_id,
                                        &sym,
                                        atomic_core::types::Side::Buy,
                                        atomic_core::types::OrderType::Market,
                                        atomic_core::types::Price(0),
                                        atomic_core::types::Qty(100_000), // 0.001 BTC
                                        atomic_core::types::TimeInForce::GoodTilCancel,
                                        0, 0,
                                    ).await;
                                });
                            }
                        }

                        // --- Emit PnL to dashboard ---
                        let drawdown_pct = if risk.peak_pnl() > 0 {
                            (risk.peak_pnl() - risk.total_pnl()) as f64 / risk.peak_pnl() as f64 * 100.0
                        } else {
                            0.0
                        };
                        let _ = dash_tx.send(DashboardEvent::Pnl {
                            total_realized: format!("{:.2}", pnl_usd),
                            total_unrealized: "0.00".to_string(),
                            total_pnl: format!("{:.2}", pnl_usd),
                            drawdown: format!("{:.2}%", drawdown_pct),
                            volume: format!("{:.2}", volume_session),
                        });

                        // --- Emit Risk to dashboard ---
                        let exposure_pipettes = (position_qty.unsigned_abs() as i64)
                            .saturating_mul(last_mid_price_pipettes)
                            / qty_divisor;
                        let drawdown_pct = if risk.peak_pnl() > 0 {
                            (risk.peak_pnl() - risk.total_pnl()) as f64 / risk.peak_pnl() as f64 * 100.0
                        } else {
                            0.0
                        };
                        let _ = dash_tx.send(DashboardEvent::Risk {
                            open_orders: risk.open_order_count() as u64,
                            max_open: config.risk.max_open_orders as u64,
                            daily_loss: format!("{:.2}", risk.total_pnl() as f64 / price_divisor),
                            max_daily_loss: format!("{:.2}", config.risk.max_loss as f64 / price_divisor),
                            exposure: format!("{:.2}", exposure_pipettes as f64 / price_divisor),
                            max_exposure: format!("{:.2}", config.risk.max_notional as f64 / price_divisor),
                            drawdown_pct: format!("{:.2}", drawdown_pct),
                            max_drawdown_pct: format!("{:.2}", config.risk.max_drawdown_bps as f64 / 100.0),
                        });
                    }
                    EventPayload::OrderReject(reject) => {
                        metrics.total_rejects.fetch_add(1, Ordering::Relaxed);
                        risk.on_order_closed();
                        order_send_times.remove(&reject.order_id.0);
                    }
                    EventPayload::OrderCancel(cancel) => {
                        risk.on_order_closed();
                        order_send_times.remove(&cancel.order_id.0);
                    }
                    EventPayload::OrderAck(ack) => {
                        let latency_ns = order_send_times.get(&ack.order_id.0)
                            .map(|t| t.elapsed().as_nanos() as u64)
                            .unwrap_or(0);
                        if latency_ns > 0 {
                            metrics.order_to_ack.record(latency_ns);
                        }
                        // Update order in dashboard with latency
                        let _ = dash_tx.send(DashboardEvent::Order {
                            id: ack.order_id.0.clone(),
                            symbol: execution.get_order(&ack.order_id.0).map(|o| format!("{}{}", o.symbol.base, o.symbol.quote)).unwrap_or_default(),
                            side: execution.get_order(&ack.order_id.0).map(|o| format!("{:?}", o.side)).unwrap_or_default(),
                            order_type: execution.get_order(&ack.order_id.0).map(|o| format!("{:?}", o.order_type)).unwrap_or_default(),
                            price: execution.get_order(&ack.order_id.0).map(|o| format!("{:.2}", o.price.to_f64(price_decimals))).unwrap_or_default(),
                            qty: execution.get_order(&ack.order_id.0).map(|o| format!("{:.8}", o.original_qty.to_f64(qty_decimals))).unwrap_or_default(),
                            filled_qty: execution.get_order(&ack.order_id.0).map(|o| format!("{:.8}", o.filled_qty.to_f64(qty_decimals))).unwrap_or_default(),
                            state: "Acked".to_string(),
                            venue: format!("{}", ack.venue),
                            latency_ns,
                        });
                        info!("ORDER ACK: {} latency={:.2}ms", ack.order_id, latency_ns as f64 / 1_000_000.0);
                    }
                    _ => {}
                }
            }

            // --- Mesh messages from peer nodes ---
            Some(msg) = async {
                match &mut mesh_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handle_mesh_message(
                    msg,
                    &mut execution,
                    &mut event_log,
                    &verifier,
                    &mesh,
                    &node_id_bytes,
                    &recovery,
                    &dash_tx,
                ).await;
            }

            // --- Heartbeat timer ---
            _ = heartbeat_interval.tick() => {
                let heartbeat = MeshMessage::Heartbeat {
                    node_id: node_id_bytes,
                    last_seq,
                    state_hash: verifier.last_hash(),
                    uptime_secs: wall_clock.now_nanos() / 1_000_000_000,
                };
                mesh.broadcast(&heartbeat).await;

                // Daily risk reset at UTC midnight
                let current_day = wall_clock.now_nanos() / 86_400_000_000_000;
                if current_day > last_reset_day {
                    last_reset_day = current_day;
                    risk.daily_reset();
                    volume_session = 0.0;
                    last_flat_pnl = 0;
                    realized_pnl = 0;
                    cost_basis = 0;
                    position_qty = 0;
                }

                // Push dashboard tick
                let rate_elapsed = last_rate_check.elapsed().as_secs_f64();
                if rate_elapsed >= 1.0 {
                    events_per_sec = (msg_count - last_msg_count) as f64 / rate_elapsed;
                    last_msg_count = msg_count;
                    last_rate_check = std::time::Instant::now();
                }
                let _ = dash_tx.send(DashboardEvent::Tick {
                    seq: last_seq,
                    events: metrics.total_events.load(Ordering::Relaxed),
                    orders: metrics.total_orders.load(Ordering::Relaxed),
                    fills: metrics.total_fills.load(Ordering::Relaxed),
                    rejects: metrics.total_rejects.load(Ordering::Relaxed),
                    uptime_secs: wall_clock.now_nanos() / 1_000_000_000,
                    state_hash: hex::encode(verifier.last_hash()),
                    msg_per_sec: events_per_sec,
                    last_price: last_price_display.clone(),
                    volume_session: format!("{:.2}", volume_session),
                });

                // Push self NodeStatus
                let _ = dash_tx.send(DashboardEvent::NodeStatus {
                    node_id: hex::encode(&node_id_bytes[..8]),
                    role: format!("{:?}", config.role),
                    status: "Online".to_string(),
                    last_seq,
                    state_hash: hex::encode(verifier.last_hash()),
                    latency_ms: 0,
                });

                // Push Risk state on heartbeat
                let drawdown_pct = if risk.peak_pnl() > 0 {
                    (risk.peak_pnl() - risk.total_pnl()) as f64 / risk.peak_pnl() as f64 * 100.0
                } else { 0.0 };
                let exposure_pipettes = (position_qty.unsigned_abs() as i64)
                    .saturating_mul(last_mid_price_pipettes)
                    / qty_divisor;
                let _ = dash_tx.send(DashboardEvent::Risk {
                    open_orders: risk.open_order_count() as u64,
                    max_open: config.risk.max_open_orders as u64,
                    daily_loss: format!("{:.2}", risk.total_pnl() as f64 / price_divisor),
                    max_daily_loss: format!("{:.2}", config.risk.max_loss as f64 / price_divisor),
                    exposure: format!("{:.2}", exposure_pipettes as f64 / price_divisor),
                    max_exposure: format!("{:.2}", config.risk.max_notional as f64 / price_divisor),
                    drawdown_pct: format!("{:.2}", drawdown_pct),
                    max_drawdown_pct: format!("{:.2}", config.risk.max_drawdown_bps as f64 / 100.0),
                });

                // Push Strategy info on heartbeat
                let _ = dash_tx.send(DashboardEvent::Strategy {
                    id: "market-maker-as".to_string(),
                    symbol: primary_symbol_code.clone(),
                    enabled: true,
                    signals: metrics.total_events.load(Ordering::Relaxed),
                    orders: metrics.total_orders.load(Ordering::Relaxed),
                    fills: metrics.total_fills.load(Ordering::Relaxed),
                    pnl: risk.total_pnl().to_string(),
                });

                // Periodic snapshot
                if last_seq > 0 && last_seq % config.snapshot_interval == 0 {
                    let snapshot = execution.snapshot(last_seq, wall_clock.now_nanos());
                    if let Err(e) = recovery.save_snapshot(&snapshot) {
                        error!("Failed to save periodic snapshot: {}", e);
                    } else {
                        info!("Snapshot saved at seq={}", last_seq);
                    }
                }
            }

            // --- Shutdown signal ---
            _ = &mut shutdown_rx => {
                info!("Shutdown signal received.");
                break;
            }
        }
    }

    // === SHUTDOWN ===
    info!("Draining — cancelling open orders on exchange...");
    if let Some(ref gw) = gateway {
        for oid in execution.open_order_ids() {
            let sym = execution
                .get_order(&oid.0)
                .map(|o| o.symbol.clone())
                .unwrap_or_else(|| primary_symbol_obj.clone());
            gw.cancel_order(&oid, &sym, last_seq, wall_clock.now_nanos()).await;
        }
        info!("All open orders cancelled.");
    }

    let _ = event_log.flush();

    // Save final snapshot
    info!("Saving final snapshot...");
    let snapshot = execution.snapshot(last_seq, wall_clock.now_nanos());
    if let Err(e) = recovery.save_snapshot(&snapshot) {
        error!("Failed to save snapshot: {}", e);
    }

    strategy_engine.stop_all();

    if cli.metrics {
        println!("\n{}", metrics.report());
    }

    info!(
        "Node stopped. Events: {} Orders: {} Fills: {} Final hash: {}",
        metrics.total_events.load(Ordering::Relaxed),
        metrics.total_orders.load(Ordering::Relaxed),
        metrics.total_fills.load(Ordering::Relaxed),
        hex::encode(execution.state_hash()),
    );
}

/// Full deterministic replay: processes events through BOTH
/// StrategyEngine AND ExecutionEngine, with periodic state verification.
fn run_replay(
    path: &str,
    execution: &mut ExecutionEngine,
    strategy_engine: &mut StrategyEngine,
    verifier: &mut StateVerifier,
    metrics: &PipelineMetrics,
) {
    let mut player = match atomic_replay::ReplayPlayer::from_file(path) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to load replay file: {}", e);
            std::process::exit(1);
        }
    };

    info!("Loaded {} events for replay", player.total_events());

    let mut processed = 0u64;
    let replay_start = std::time::Instant::now();

    while let Some(event) = player.next() {
        // Process through execution engine
        let _timer = atomic_core::metrics::StageTimer::start(&metrics.event_processing);
        let _ = execution.process_event(event);

        // Process through strategy engine
        let _stimer = atomic_core::metrics::StageTimer::start(&metrics.strategy_compute);
        let _commands = strategy_engine.process_event(event);
        drop(_stimer);

        drop(_timer);
        processed += 1;
        metrics.total_events.fetch_add(1, Ordering::Relaxed);

        // Count event types
        match &event.payload {
            EventPayload::OrderNew(_) => { metrics.total_orders.fetch_add(1, Ordering::Relaxed); }
            EventPayload::OrderFill(_) | EventPayload::OrderPartialFill(_) => {
                metrics.total_fills.fetch_add(1, Ordering::Relaxed);
            }
            EventPayload::OrderReject(_) => { metrics.total_rejects.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }

        // Periodic state verification
        if verifier.tick() {
            let _htimer = atomic_core::metrics::StageTimer::start(&metrics.state_hash);
            let hash = execution.state_hash();
            verifier.on_verified(event.seq, hash);
            drop(_htimer);
        }

        if processed % 100_000 == 0 {
            info!(
                "Replayed {}/{} events ({:.1}%) — hash: {}",
                processed,
                player.total_events(),
                player.progress_pct(),
                hex::encode(verifier.last_hash()),
            );
        }
    }

    let elapsed = replay_start.elapsed();
    let eps = if elapsed.as_secs_f64() > 0.0 {
        processed as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    info!("Replay complete: {} events in {:.2}s ({:.0} events/sec)", processed, elapsed.as_secs_f64(), eps);
    info!("Final state hash: {}", hex::encode(execution.state_hash()));
}

/// Backtest mode: replay market data through C++ hot-path engine + simulated exchange.
/// Produces PnL tracking, equity curve, and a full performance report.
fn run_backtest(
    path: &str,
    execution: &mut ExecutionEngine,
    verifier: &mut StateVerifier,
    metrics: &PipelineMetrics,
    config: &NodeConfig,
    equity_csv: Option<&str>,
) {
    let mut player = match atomic_replay::ReplayPlayer::from_file(path) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to load backtest file: {}", e);
            std::process::exit(1);
        }
    };

    info!("Loaded {} events for backtest", player.total_events());

    // Decimal precision from config
    let price_decimals: u8 = config.feeds.first().map(|f| f.price_decimals).unwrap_or(2);
    let qty_decimals: u8 = config.feeds.first().map(|f| f.qty_decimals).unwrap_or(8);
    let qty_divisor = 10i64.pow(qty_decimals as u32);
    let price_divisor = 10f64.powi(price_decimals as i32);

    // C++ hot-path engine — aggressive inventory management for more round-trips
    let mut hotpath = HotPathEngine::new(
        10_000_000,    // order_qty = 0.1 BTC ($7,320 notional)
        10_000_000,    // max_inventory = 0.1 BTC (1 unit for clean round-trips)
        80,            // half_spread = 80 pipettes ($0.80 per side)
        5000,          // gamma = 0.5
        3,             // warmup_ticks
        5,             // cooldown_ticks: fast requoting when flat
        200,           // requote_threshold
        false,         // vpin_enabled: off for cleaner backtest
    );
    info!("C++ hot-path engine initialized for backtest");

    // Maker fee = 0 bps (realistic for Binance VIP/MM program tier)
    let sim_config = SimulatorConfig {
        maker_fee_bps: 0,
        ..SimulatorConfig::default()
    };
    let mut sim = SimulatedExchange::new(sim_config);
    let mut processed = 0u64;
    let source = [0u8; 32];
    let replay_start = std::time::Instant::now();

    // PnL tracking — use f64 to avoid integer truncation on small notionals
    let mut position_qty: i64 = 0;
    let mut cost_basis: f64 = 0.0; // in USD
    let mut realized_pnl: f64 = 0.0; // in USD
    let mut total_trades: u64 = 0;
    let mut winning_trades: u64 = 0;
    let mut peak_pnl: f64 = 0.0;
    let mut max_drawdown: f64 = 0.0;
    let mut volume_total: f64 = 0.0;

    // Equity curve: (event_seq, realized_pnl_usd, position_qty)
    let mut equity_curve: Vec<(u64, f64, i64)> = Vec::new();

    // Per-trade tracking for win/loss
    let mut trade_pnls: Vec<f64> = Vec::new();
    let mut last_flat_pnl: f64 = 0.0;

    let fallback_symbol = config
        .feeds
        .first()
        .and_then(|f| f.symbols.first().map(|s| parse_venue_symbol(s, f.venue)))
        .unwrap_or_else(|| parse_venue_symbol("BTCUSDT", Venue::Simulated));

    while let Some(event) = player.next() {
        let order_symbol = match &event.payload {
            EventPayload::OrderBookUpdate(obu) => obu.symbol.clone(),
            EventPayload::Trade(t) => t.symbol.clone(),
            _ => fallback_symbol.clone(),
        };

        // Feed market data to simulator
        if let EventPayload::OrderBookUpdate(ref obu) = event.payload {
            sim.on_book_update(&obu.symbol, &obu.bids, &obu.asks, obu.is_snapshot);
        }

        // Route event through C++ hot-path engine
        let hp_result = match &event.payload {
            EventPayload::OrderBookUpdate(obu) => {
                hotpath.on_book_update(&obu.bids, &obu.asks, obu.is_snapshot)
            }
            EventPayload::Trade(t) => {
                hotpath.on_trade(t.side, t.price, t.qty)
            }
            _ => atomic_hotpath::HpResult::default(),
        };

        // Only submit new orders when flat — hold existing quotes when positioned
        if position_qty == 0 {
        for i in 0..hp_result.count {
            let cmd = &hp_result.commands[i as usize];
            match cmd.tag {
                1 => { // HP_CMD_PLACE_BID
                    let order_id = execution.next_order_id(Venue::Simulated);
                    let order_new = atomic_core::event::OrderNewEvent {
                        order_id,
                        symbol: order_symbol.clone(),
                        side: atomic_core::types::Side::Buy,
                        order_type: atomic_core::types::OrderType::Limit,
                        price: atomic_core::types::Price(cmd.price),
                        qty: atomic_core::types::Qty(cmd.qty),
                        time_in_force: atomic_core::types::TimeInForce::GoodTilCancel,
                        venue: Venue::Simulated,
                    };
                    let order_event = Event::new(
                        event.seq * 1000 + i as u64,
                        event.timestamp,
                        source,
                        EventPayload::OrderNew(order_new.clone()),
                    );
                    let _ = execution.process_event(&order_event);
                    sim.submit_order(&order_new, event.timestamp, source);
                    metrics.total_orders.fetch_add(1, Ordering::Relaxed);
                }
                2 => { // HP_CMD_PLACE_ASK
                    let order_id = execution.next_order_id(Venue::Simulated);
                    let order_new = atomic_core::event::OrderNewEvent {
                        order_id,
                        symbol: order_symbol.clone(),
                        side: atomic_core::types::Side::Sell,
                        order_type: atomic_core::types::OrderType::Limit,
                        price: atomic_core::types::Price(cmd.price),
                        qty: atomic_core::types::Qty(cmd.qty),
                        time_in_force: atomic_core::types::TimeInForce::GoodTilCancel,
                        venue: Venue::Simulated,
                    };
                    let order_event = Event::new(
                        event.seq * 1000 + i as u64,
                        event.timestamp,
                        source,
                        EventPayload::OrderNew(order_new.clone()),
                    );
                    let _ = execution.process_event(&order_event);
                    sim.submit_order(&order_new, event.timestamp, source);
                    metrics.total_orders.fetch_add(1, Ordering::Relaxed);
                }
                3 => { // HP_CMD_CANCEL_ALL
                    // Only cancel when flat — keep unfilled side alive after partial fill
                    if position_qty == 0 {
                        let open_ids = execution.open_order_ids();
                        for oid in &open_ids {
                            sim.cancel_order(oid, event.timestamp, source);
                        }
                    }
                }
                _ => {}
            }
        }
        } // end: only submit new orders when flat

        // Drain simulated exchange events → process fills
        let sim_events = sim.drain_events(event.seq * 1000);
        for sim_event in &sim_events {
            let _ = execution.process_event(sim_event);

            match &sim_event.payload {
                EventPayload::OrderFill(fill) | EventPayload::OrderPartialFill(fill) => {
                    metrics.total_fills.fetch_add(1, Ordering::Relaxed);

                    // Feed fill back to C++ engine for inventory tracking
                    hotpath.on_fill(fill.side, fill.qty, fill.price);

                    // PnL computation with f64 to avoid integer truncation
                    let fill_price_usd = fill.price.0 as f64 / price_divisor;
                    let fill_qty_units = fill.qty.0 as f64 / qty_divisor as f64;
                    let fill_notional = fill_price_usd * fill_qty_units;
                    let fill_qty = fill.qty.0 as i64;

                    match fill.side {
                        atomic_core::types::Side::Buy => {
                            if position_qty >= 0 {
                                position_qty += fill_qty;
                                cost_basis += fill_notional;
                            } else {
                                let close_qty = fill_qty.min(-position_qty);
                                let close_frac = close_qty as f64 / (-position_qty) as f64;
                                let close_cost = cost_basis * close_frac;
                                let close_notional = fill_price_usd * (close_qty as f64 / qty_divisor as f64);
                                realized_pnl += close_cost.abs() - close_notional; // short close: entry - exit
                                position_qty += fill_qty;
                                cost_basis -= close_cost;
                                let open_qty = fill_qty - close_qty;
                                if open_qty > 0 {
                                    cost_basis = fill_price_usd * (open_qty as f64 / qty_divisor as f64);
                                }
                            }
                        }
                        atomic_core::types::Side::Sell => {
                            if position_qty <= 0 {
                                position_qty -= fill_qty;
                                cost_basis -= fill_notional;
                            } else {
                                let close_qty = fill_qty.min(position_qty);
                                let close_frac = close_qty as f64 / position_qty as f64;
                                let close_cost = cost_basis * close_frac;
                                let close_notional = fill_price_usd * (close_qty as f64 / qty_divisor as f64);
                                realized_pnl += close_notional - close_cost; // long close: exit - entry
                                position_qty -= fill_qty;
                                cost_basis -= close_cost;
                                let open_qty = fill_qty - close_qty;
                                if open_qty > 0 {
                                    cost_basis = -(fill_price_usd * (open_qty as f64 / qty_divisor as f64));
                                }
                            }
                        }
                    }
                    let fee_usd = fill.fee as f64 / qty_divisor as f64 / price_divisor;
                    realized_pnl -= fee_usd;
                    volume_total += fill_notional.abs();

                    // Track round-trip trades when position goes flat
                    if position_qty == 0 {
                        cost_basis = 0.0;
                        let trade_pnl = realized_pnl - last_flat_pnl;
                        trade_pnls.push(trade_pnl);
                        total_trades += 1;
                        if trade_pnl > 0.0 {
                            winning_trades += 1;
                        }
                        last_flat_pnl = realized_pnl;
                    }

                    // Drawdown tracking
                    if realized_pnl > peak_pnl {
                        peak_pnl = realized_pnl;
                    }
                    let dd = peak_pnl - realized_pnl;
                    if dd > max_drawdown {
                        max_drawdown = dd;
                    }

                    // Record equity point on every fill
                    equity_curve.push((event.seq, realized_pnl, position_qty));
                }
                _ => {}
            }
        }

        processed += 1;
        metrics.total_events.fetch_add(1, Ordering::Relaxed);

        if verifier.tick() {
            let hash = execution.state_hash();
            verifier.on_verified(event.seq, hash);
        }

        if processed % 100_000 == 0 {
            info!(
                "Backtest {}/{} ({:.1}%) — orders: {} fills: {} resting: {} PnL: ${:.4}",
                processed,
                player.total_events(),
                player.progress_pct(),
                metrics.total_orders.load(Ordering::Relaxed),
                metrics.total_fills.load(Ordering::Relaxed),
                sim.resting_order_count(),
                realized_pnl,
            );
        }
    }

    let elapsed = replay_start.elapsed();
    let total_fills = metrics.total_fills.load(Ordering::Relaxed);
    let total_orders = metrics.total_orders.load(Ordering::Relaxed);
    let pnl_usd = realized_pnl;
    let dd_usd = max_drawdown;
    let eps = if elapsed.as_secs_f64() > 0.0 { processed as f64 / elapsed.as_secs_f64() } else { 0.0 };

    // Compute Sharpe ratio from round-trip trade PnLs
    let (avg_trade, sharpe) = if trade_pnls.len() >= 2 {
        let mean = trade_pnls.iter().sum::<f64>() / trade_pnls.len() as f64;
        let variance = trade_pnls.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (trade_pnls.len() - 1) as f64;
        let std_dev = variance.sqrt();
        let s = if std_dev > 1e-12 { (mean / std_dev).min(999.999) } else { 0.0 };
        (mean, s)
    } else {
        (pnl_usd, 0.0)
    };

    let win_rate = if total_trades > 0 { winning_trades as f64 / total_trades as f64 * 100.0 } else { 0.0 };

    // Export equity curve CSV
    if let Some(csv_path) = equity_csv {
        match std::fs::File::create(csv_path) {
            Ok(file) => {
                use std::io::Write;
                let mut w = std::io::BufWriter::new(file);
                let _ = writeln!(w, "seq,realized_pnl_usd,position_qty");
                for (seq, pnl, pos) in &equity_curve {
                    let _ = writeln!(w, "{},{:.6},{}", seq, pnl, pos);
                }
                info!("Equity curve exported to {}", csv_path);
            }
            Err(e) => {
                error!("Failed to write equity CSV: {}", e);
            }
        }
    }

    // Print comprehensive report
    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║              ATOMIC MESH — BACKTEST REPORT              ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Engine          : C++ HotPath (Avellaneda-Stoikov)     ║");
    println!("║  Events          : {:>10} ({:.0} evt/s){:>15}║", processed, eps, "");
    println!("║  Duration        : {:.2}s{:>36}║", elapsed.as_secs_f64(), "");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Orders          : {:>10}{:>28}║", total_orders, "");
    println!("║  Fills           : {:>10}{:>28}║", total_fills, "");
    println!("║  Round-trips     : {:>10}{:>28}║", total_trades, "");
    println!("║  Volume          : ${:>12.2}{:>24}║", volume_total, "");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Realized PnL    : ${:>12.4}{:>24}║", pnl_usd, "");
    println!("║  Max Drawdown    : ${:>12.4}{:>24}║", dd_usd, "");
    println!("║  Win Rate        : {:>11.1}%{:>25}║", win_rate, "");
    println!("║  Avg Trade PnL   : ${:>12.6}{:>24}║", avg_trade, "");
    println!("║  Sharpe (trade)  : {:>12.3}{:>25}║", sharpe, "");
    println!("║  Final Position  : {:>10}{:>28}║", position_qty, "");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  State Hash      : {}  ║", hex::encode(execution.state_hash()));
    println!("╚══════════════════════════════════════════════════════════╝\n");
}

/// Handle incoming mesh messages from peer nodes.
async fn handle_mesh_message(
    msg: MeshMessage,
    execution: &mut ExecutionEngine,
    event_log: &mut EventLog,
    verifier: &StateVerifier,
    mesh: &MeshTransport,
    _node_id: &[u8; 32],
    recovery: &RecoveryCoordinator,
    dash_tx: &tokio::sync::broadcast::Sender<DashboardEvent>,
) {
    match msg {
        MeshMessage::EventReplication(event) => {
            let _ = execution.process_event(&event);
            let _ = event_log.append(&event);
            info!("Replicated event seq={}", event.seq);
        }

        MeshMessage::EventBatch(events) => {
            for event in &events {
                let _ = execution.process_event(event);
                let _ = event_log.append(event);
            }
            info!("Replicated batch of {} events", events.len());
        }

        MeshMessage::Heartbeat { node_id: peer_id, last_seq, state_hash, uptime_secs } => {
            let peer_hex = hex::encode(&peer_id[..8]);
            tracing::debug!(
                "Heartbeat from {} — seq={} hash={} uptime={}s",
                peer_hex, last_seq, hex::encode(state_hash), uptime_secs
            );

            let matches = verifier.verify_peer(last_seq, state_hash);
            if !matches {
                error!(
                    "STATE DIVERGENCE with peer {} at seq={} — local={} remote={}",
                    peer_hex, last_seq,
                    hex::encode(verifier.last_hash()),
                    hex::encode(state_hash),
                );
            }

            // Emit NodeStatus for peer
            let _ = dash_tx.send(DashboardEvent::NodeStatus {
                node_id: peer_hex.clone(),
                role: "Follower".to_string(),
                status: "Online".to_string(),
                last_seq,
                state_hash: hex::encode(state_hash),
                latency_ms: 0,
            });

            // Emit HashCheck
            let _ = dash_tx.send(DashboardEvent::HashCheck {
                node_id: peer_hex,
                seq: last_seq,
                hash: hex::encode(state_hash),
                matches,
            });
        }

        MeshMessage::SyncRequest { from_seq, to_seq } => {
            info!("SyncRequest from peer: seq {}..{}", from_seq, to_seq);
            let log = EventLog::read_only(&recovery.event_log_path());
            match log.read_range(from_seq, to_seq) {
                Ok(events) => {
                    let response = MeshMessage::SyncResponse { events };
                    mesh.broadcast(&response).await;
                    info!("SyncResponse sent");
                }
                Err(e) => {
                    warn!("Failed to read events for sync: {}", e);
                }
            }
        }

        MeshMessage::SyncResponse { events } => {
            info!("SyncResponse received: {} events", events.len());
            for event in &events {
                let _ = execution.process_event(event);
                let _ = event_log.append(event);
            }
        }

        MeshMessage::HashVerify { seq, hash } => {
            let matches = verifier.verify_peer(seq, hash);
            if !matches {
                error!(
                    "HASH MISMATCH at seq={} — local={} remote={}",
                    seq, hex::encode(verifier.last_hash()), hex::encode(hash)
                );
            }

            let _ = dash_tx.send(DashboardEvent::HashCheck {
                node_id: "peer".to_string(),
                seq,
                hash: hex::encode(hash),
                matches,
            });
        }

        MeshMessage::SnapshotBroadcast(snapshot) => {
            info!("Received snapshot broadcast at seq={}", snapshot.seq);
            if let Err(e) = recovery.save_snapshot(&snapshot) {
                warn!("Failed to save received snapshot: {}", e);
            }
        }

        MeshMessage::ConsensusVote { proposal_seq, node_id: voter, approved } => {
            info!(
                "Consensus vote from {}: seq={} approved={}",
                hex::encode(&voter[..8]), proposal_seq, approved
            );
        }

        MeshMessage::NodeAnnounce { node_id: peer_id, role, addr } => {
            info!(
                "Node announced: {} role={:?} addr={}",
                hex::encode(&peer_id[..8]), role, addr
            );
        }
    }
}
