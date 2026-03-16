use clap::Parser;
use std::sync::atomic::Ordering;
use tracing::{info, warn, error};

use atomic_core::clock::{LamportClock, SequenceGenerator, WallClock};
use atomic_core::event::{Event, EventPayload};
use atomic_core::metrics::PipelineMetrics;
use atomic_core::snapshot::EngineSnapshot;
use atomic_bus::EventSequencer;
use atomic_execution::{ExecutionEngine, SimulatedExchange, SimulatorConfig, StateVerifier};
use atomic_orderbook::OrderBook;
use atomic_risk::RiskEngine;
use atomic_strategy::StrategyEngine;
use atomic_node::{NodeConfig, RecoveryCoordinator, EventDeduplicator};

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

    /// Print metrics report at end of replay/backtest
    #[arg(long)]
    metrics: bool,
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

    // Initialize metrics
    let metrics = PipelineMetrics::new();

    // Initialize components
    let clock = LamportClock::new();
    let wall_clock = WallClock::live();
    let mut sequencer = EventSequencer::new(1_000_000);
    let mut execution = ExecutionEngine::new();
    let mut strategy_engine = StrategyEngine::new();
    let risk = RiskEngine::new(config.risk.clone());
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
        warn!("Event log has gaps — peer sync needed from seq {}", plan.sync_from_seq);
        warn!("In production, would send SyncRequest to peers here");
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
        run_backtest(&backtest_path, &mut execution, &mut strategy_engine, &mut verifier, &metrics);

        if cli.metrics {
            println!("\n{}", metrics.report());
        }
        return;
    }

    // === LIVE MODE ===
    info!("Starting in LIVE mode...");
    info!("Node is ready. Press Ctrl+C to shutdown.");
    tokio::signal::ctrl_c().await.unwrap();

    // Save snapshot on shutdown
    info!("Shutdown signal received. Saving snapshot...");
    let seq = verifier.last_seq();
    let snapshot = execution.snapshot(seq, wall_clock.now_nanos());
    if let Err(e) = recovery.save_snapshot(&snapshot) {
        error!("Failed to save snapshot: {}", e);
    }

    strategy_engine.stop_all();
    info!("Node stopped cleanly. Final state hash: {}", hex::encode(execution.state_hash()));
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

/// Backtest mode: replay market data through a simulated exchange.
fn run_backtest(
    path: &str,
    execution: &mut ExecutionEngine,
    strategy_engine: &mut StrategyEngine,
    verifier: &mut StateVerifier,
    metrics: &PipelineMetrics,
) {
    let mut player = match atomic_replay::ReplayPlayer::from_file(path) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to load backtest file: {}", e);
            std::process::exit(1);
        }
    };

    info!("Loaded {} events for backtest", player.total_events());

    let mut sim = SimulatedExchange::new(SimulatorConfig::default());
    let mut processed = 0u64;
    let source = [0u8; 32];
    let replay_start = std::time::Instant::now();

    while let Some(event) = player.next() {
        // Feed market data to simulator
        match &event.payload {
            EventPayload::OrderBookUpdate(obu) => {
                sim.on_book_update(&obu.symbol, &obu.bids, &obu.asks, obu.is_snapshot);
            }
            _ => {}
        }

        // Process through strategy engine → get commands
        let commands = strategy_engine.process_event(event);

        // Execute strategy commands via simulated exchange
        for cmd in commands {
            match cmd {
                atomic_strategy::StrategyCommand::PlaceOrder {
                    symbol, side, order_type, price, qty, time_in_force, venue: _,
                } => {
                    let order_id = execution.next_order_id(atomic_core::types::Venue::Simulated);
                    let order_new = atomic_core::event::OrderNewEvent {
                        order_id,
                        symbol,
                        side,
                        order_type,
                        price,
                        qty,
                        time_in_force,
                        venue: atomic_core::types::Venue::Simulated,
                    };
                    sim.submit_order(&order_new, event.timestamp, source);
                    metrics.total_orders.fetch_add(1, Ordering::Relaxed);
                }
                atomic_strategy::StrategyCommand::CancelOrder { order_id } => {
                    sim.cancel_order(&order_id, event.timestamp, source);
                }
                atomic_strategy::StrategyCommand::CancelAll { .. } => {}
            }
        }

        // Process simulated exchange events through execution engine
        let sim_events = sim.drain_events(event.seq * 1000); // offset to avoid seq collision
        for sim_event in &sim_events {
            let _ = execution.process_event(sim_event);
            match &sim_event.payload {
                EventPayload::OrderFill(_) | EventPayload::OrderPartialFill(_) => {
                    metrics.total_fills.fetch_add(1, Ordering::Relaxed);
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
                "Backtest {}/{} ({:.1}%) — orders: {} fills: {} resting: {}",
                processed,
                player.total_events(),
                player.progress_pct(),
                metrics.total_orders.load(Ordering::Relaxed),
                metrics.total_fills.load(Ordering::Relaxed),
                sim.resting_order_count(),
            );
        }
    }

    let elapsed = replay_start.elapsed();
    info!(
        "Backtest complete: {} events in {:.2}s — orders: {} fills: {}",
        processed, elapsed.as_secs_f64(),
        metrics.total_orders.load(Ordering::Relaxed),
        metrics.total_fills.load(Ordering::Relaxed),
    );
    info!("Final state hash: {}", hex::encode(execution.state_hash()));
}
