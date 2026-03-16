# ATOMIC MESH

**Distributed Deterministic Trading Engine**

A multi-node, event-sourced trading engine where every state change is an event, every event has a global sequence number, and replaying the same events always produces the same state. Built in Rust for maximum performance and safety.

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │              ATOMIC MESH NODE               │
                    │                                             │
  Exchange WS ────►│  Feed ──► Bus ──► Strategy ──► Router       │
  (Binance,        │  Handler  (SPSC   Engine       (SOR)        │
   Bybit, ...)     │  + Norm   Ring)     │            │          │
                    │           │         │            ▼          │
                    │        Metrics   Metrics    Execution ◄── Risk
                    │        (per-     (per-      Engine       Engine
                    │         stage)    stage)       │          │
                    │                     ▼          ▼          │
                    │               Event Log    Exchange API   │
                    │              (append-only)  (Live/Sim)    │
                    │                     │                     │
                    │              State Verifier               │
                    │              (xxHash3 periodic)           │
                    └───────────┬─────────────────────┬────────┘
                                │  QUIC Transport     │
                    ┌───────────▼─────────────────────▼────────┐
                    │  Peer Nodes (Replication + Recovery)     │
                    └─────────────────────────────────────────-┘
```

## Core Principles

1. **Determinism** — Same events → same state. Always. No unseeded random, no wall-clock in hot path, single-threaded event processing. **Proven by test**: replay twice → identical xxHash3.
2. **Event Sourcing** — Every state change is an immutable event with a monotonic sequence number. The event log IS the database.
3. **State Verification** — `StateVerifier` computes `xxHash3(engine_state)` every N events. Cross-node hash comparison via `HashVerify` messages. Divergence → halt.
4. **Replay** — `ExecutionEngine::process_event()` replays the full order lifecycle. Snapshot → restore → verify produces identical state hash.

## Crate Structure

| Crate | Purpose |
|---|---|
| `atomic-core` | Events, types (Price/Qty as integers), Lamport clock, snapshot, **pipeline metrics** |
| `atomic-bus` | SPSC lock-free ring buffer, event sequencer |
| `atomic-feed` | Exchange WebSocket connectors (Binance, ...), feed normalizer |
| `atomic-orderbook` | BTreeMap-based L2 order book engine |
| `atomic-strategy` | Strategy trait + engine (deterministic, no I/O in hot path) |
| `atomic-router` | Smart Order Router: BestVenue, VWAP, TWAP, LiquiditySweep |
| `atomic-risk` | Position limits, max order size, rate limits, kill switch |
| `atomic-execution` | Order state machine, **simulated exchange**, state hash/snapshot/restore |
| `atomic-replay` | Deterministic replay from event log, seek, batch, **idempotency tests** |
| `atomic-transport` | QUIC encrypted inter-node mesh (event replication, consensus) |
| `atomic-node` | CLI entry point, config, **recovery coordinator**, **backtest mode** |

## 5 Key Differentiators

These features put ATOMIC MESH above single-node research engines like NautilusTrader and Lean:

### 1. Deterministic Replay (Proven)

Replay processes events through the **full execution engine** (not just strategy). `ExecutionEngine::process_event()` handles OrderNew, OrderAck, OrderFill, OrderCancel, OrderReject. Two tests prove determinism:

- `replay_determinism_same_events_same_hash` — Replay the same events twice → identical state hash
- `replay_snapshot_restore_same_hash` — Snapshot → restore → same hash

### 2. State Hash Verification

`StateVerifier` ticks every N events and triggers state hash computation:

- `ExecutionEngine::serialize_state()` — Deterministic serialization (orders sorted by ID)
- `ExecutionEngine::state_hash()` — xxHash3 of serialized state
- `StateVerifier::verify_peer(seq, hash)` — Cross-node comparison
- Divergence detection: if peer hash differs at same seq → bug detected

### 3. Exchange Simulator

`SimulatedExchange` — Full matching engine for backtesting without live connections:

- **Market orders**: Walk the book, consume liquidity, generate fills
- **Limit orders**: Cross the book or rest; resting orders fill when book updates
- **Cancel support**: Remove resting orders
- **Configurable fees**: Separate maker/taker in basis points
- **Configurable latency**: Ack delay + fill delay in nanoseconds
- **Backtest mode**: `--backtest events.log` replays market data through simulator
- 5 tests: market buy/sell, limit rest, resting fill on update, cancel

### 4. Latency Observability

`PipelineMetrics` — Zero-allocation lock-free metrics using atomic counters:

- **10 histograms**: feed_recv, feed_normalize, ring_enqueue, strategy_compute, risk_check, order_submit, order_to_ack, order_to_fill, event_processing, state_hash
- **5 counters**: total_events, total_orders, total_fills, total_rejects, sequence_gaps
- **LatencyHistogram**: 10 fixed buckets (100ns → 10s), min/max/avg, percentile approximation
- **StageTimer**: RAII timer that records to histogram on drop
- **Report**: `--metrics` flag prints full report at end of replay/backtest

### 5. Distributed Recovery

`RecoveryCoordinator` — Crash recovery and state restoration:

- **Snapshot persistence**: `save_snapshot()` writes EngineSnapshot to disk (bincode)
- **Snapshot loading**: `load_latest_snapshot()` with hash verification on load
- **Recovery planning**: `plan_recovery()` scans snapshot + event log, detects gaps
- **Gap detection**: Finds missing seqs between snapshot and event log
- **Event deduplication**: `EventDeduplicator` prevents processing events twice during recovery
- **Shutdown snapshot**: Node saves snapshot on Ctrl+C for fast restart
- **SyncRequest/SyncResponse**: Wire protocol messages for peer catchup (handler integration pending)

## Key Design Decisions

### Integer-Only Pricing
No floating point in the hot path. `Price(i64)` = pipettes, `Qty(u64)` = base units. All arithmetic is integer.

### Lock-Free SPSC Ring Buffer
Events flow from producer to consumer through a cache-friendly, zero-allocation ring buffer with atomic head/tail pointers.

### Lamport Clocks
Each node maintains a logical clock. On local event: `tick()`. On receive: `witness(remote_ts) → max(local, remote) + 1`. Provides causal ordering without wall-clock dependency.

### Order State Machine
Strict lifecycle enforcement:
```
New ──► Ack ──► PartialFill ──► Filled
 │       │         │
 │       │         └──► Canceled
 │       └──► Canceled
 └──► Rejected
```
Invalid transitions are rejected at runtime via `can_transition_to()` checks. `ExecutionEngine::process_event()` applies the full lifecycle from events.

### Smart Order Router
Four algorithms for cross-venue order splitting:
- **BestVenue** — Route entire order to venue with best price
- **VWAP** — Split proportional to venue liquidity depth
- **TWAP** — Split across time slices
- **LiquiditySweep** — Walk the book across all venues simultaneously

## Quick Start

```bash
# Generate default config
cargo run -- --generate-config > config.json

# Start a node
cargo run -- --config config.json

# Replay events (full execution engine path)
cargo run -- --replay data/events.log --metrics

# Backtest with simulated exchange
cargo run -- --backtest data/events.log --metrics

# Run tests (21 tests across 6 crates)
cargo test

# Build release
cargo build --release
```

## Test Coverage

| Crate | Tests | What |
|---|---|---|
| `atomic-bus` | 3 | SPSC ring buffer push/pop/wrap/full |
| `atomic-core` | 4 | Histogram record/percentile, StageTimer, PipelineMetrics report |
| `atomic-execution` | 7 | Order lifecycle, invalid transitions, market buy/sell, limit rest, resting fill, cancel |
| `atomic-node` | 2 | Event deduplicator, recovery cold start |
| `atomic-orderbook` | 3 | Book snapshot, delta, simulate_fill |
| `atomic-replay` | 2 | **Deterministic replay hash equality**, snapshot restore hash equality |
| **Total** | **21** | |

## Wire Protocol

Inter-node communication uses QUIC with self-signed TLS certificates. Messages are bincode-serialized for minimal overhead.

| Message | Purpose |
|---|---|
| `EventReplication` | Replicate a single event to followers |
| `EventBatch` | Bulk replication |
| `Heartbeat` | Node liveness + state hash |
| `SyncRequest/Response` | Catch-up for lagging nodes |
| `ConsensusVote` | Pre-execution agreement |
| `HashVerify` | Cross-node state verification |

## License

MIT
