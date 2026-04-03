//! End-to-end integration test: synthetic events → C++ HotPathEngine →
//! SimulatedExchange → PnL tracking → state-hash determinism.
//!
//! Proves the full pipeline works in-process without network or config files.

use atomic_core::event::{Event, EventPayload, OrderBookUpdateEvent, TradeEvent};
use atomic_core::types::{Level, OrderId, Price, Qty, Side, Symbol, Venue, OrderType, TimeInForce};
use atomic_core::event::OrderNewEvent;
use atomic_execution::{ExecutionEngine, SimulatedExchange, SimulatorConfig, StateVerifier};
use atomic_hotpath::{HotPathEngine, HpCmdTag};
use atomic_risk::{RiskEngine, RiskLimits};

fn sym() -> Symbol {
    Symbol::new("BTC", "USDT", Venue::Simulated)
}

/// Generate synthetic order book updates around a drifting mid-price.
fn generate_synthetic_events(count: usize) -> Vec<Event> {
    let source = [0u8; 32];
    let base_mid = 7_000_000_i64; // $70,000.00
    let mut events = Vec::with_capacity(count);
    let mut seq = 1u64;

    for i in 0..count {
        let drift = ((i as f64 * 0.01).sin() * 500.0) as i64; // oscillating mid
        let mid = base_mid + drift;

        // Book update: 10-deep each side, tightening/widening spread
        let spread_half = 50 + (i % 30) as i64; // 50..80 pipettes
        let bids: Vec<Level> = (0..10)
            .map(|d| Level {
                price: Price(mid - spread_half - d * 100),
                qty: Qty(20_000_000 + d as u64 * 3_000_000),
            })
            .collect();
        let asks: Vec<Level> = (0..10)
            .map(|d| Level {
                price: Price(mid + spread_half + d * 100),
                qty: Qty(15_000_000 + d as u64 * 2_000_000),
            })
            .collect();

        events.push(Event::new(
            seq,
            1_000_000_000 + (i as u64) * 100_000, // 100µs apart
            source,
            EventPayload::OrderBookUpdate(OrderBookUpdateEvent {
                symbol: sym(),
                bids,
                asks,
                is_snapshot: i < 3, // first 3 are snapshots
                exchange_ts: 1_000_000_000 + (i as u64) * 100_000,
            }),
        ));
        seq += 1;

        // Every 3rd event also gets a trade
        if i % 3 == 0 {
            let trade_side = if i % 6 == 0 { Side::Buy } else { Side::Sell };
            events.push(Event::new(
                seq,
                1_000_000_000 + (i as u64) * 100_000 + 50_000,
                source,
                EventPayload::Trade(TradeEvent {
                    symbol: sym(),
                    price: Price(mid),
                    qty: Qty(5_000_000),
                    side: trade_side,
                    exchange_ts: 1_000_000_000 + (i as u64) * 100_000 + 50_000,
                }),
            ));
            seq += 1;
        }
    }
    events
}

/// Run the full pipeline once and return (realized_pnl, fill_count, state_hash).
fn run_pipeline(events: &[Event]) -> (i64, u64, [u8; 8]) {
    let mut hotpath = HotPathEngine::new(
        100_000,    // order_qty
        20_000_000, // max_inventory
        10,         // half_spread_pipettes
        1000,       // gamma
        3,          // warmup
        2,          // cooldown
        5,          // requote_threshold
        true,       // vpin
    );
    let mut sim = SimulatedExchange::new(SimulatorConfig::default());
    let mut execution = ExecutionEngine::new();
    let mut risk = RiskEngine::new(RiskLimits::default());
    let mut verifier = StateVerifier::new(1000);

    let source = [0u8; 32];
    let mut position_qty: i64 = 0;
    let mut cost_basis: i64 = 0;
    let mut realized_pnl: i64 = 0;
    let mut fill_count: u64 = 0;
    let mut next_seq = events.len() as u64 + 100;

    for event in events {
        let result = match &event.payload {
            EventPayload::OrderBookUpdate(book) => {
                sim.on_book_update(&book.symbol, &book.bids, &book.asks, book.is_snapshot);
                risk.update_spread(if let (Some(b), Some(a)) = (book.bids.first(), book.asks.first()) {
                    a.price.0 - b.price.0
                } else {
                    0
                });
                hotpath.on_book_update(&book.bids, &book.asks, book.is_snapshot)
            }
            EventPayload::Trade(trade) => {
                hotpath.on_trade(trade.side, trade.price, trade.qty)
            }
            _ => continue,
        };

        // Process C++ commands → order submission
        for i in 0..result.count as usize {
            let cmd = &result.commands[i];
            let tag = cmd.tag;
            if tag == HpCmdTag::CancelAll as i32 {
                for oid in execution.open_order_ids() {
                    sim.cancel_order(&oid, event.timestamp, source);
                }
            } else if tag == HpCmdTag::PlaceBid as i32 || tag == HpCmdTag::PlaceAsk as i32 {
                let side = if tag == HpCmdTag::PlaceBid as i32 { Side::Buy } else { Side::Sell };
                let order_id = execution.next_order_id(Venue::Simulated);
                let order_new = OrderNewEvent {
                    order_id: order_id.clone(),
                    symbol: sym(),
                    side,
                    order_type: OrderType::Limit,
                    price: Price(cmd.price),
                    qty: Qty(cmd.qty),
                    time_in_force: TimeInForce::GoodTilCancel,
                    venue: Venue::Simulated,
                };
                let order_event = Event::new(
                    0,
                    event.timestamp,
                    source,
                    EventPayload::OrderNew(order_new.clone()),
                );
                let _ = execution.process_event(&order_event);
                sim.submit_order(&order_new, event.timestamp, source);
            }
        }

        // Drain simulator events → execution engine + PnL
        let sim_events = sim.drain_events(next_seq);
        next_seq += sim_events.len() as u64;
        for sim_ev in &sim_events {
            let _ = execution.process_event(sim_ev);
            if let EventPayload::OrderFill(fill) = &sim_ev.payload {
                fill_count += 1;
                let signed_qty = if fill.side == Side::Buy { fill.qty.0 as i64 } else { -(fill.qty.0 as i64) };
                let prev_pos = position_qty;
                position_qty += signed_qty;
                cost_basis += signed_qty * fill.price.0;
                hotpath.on_fill(fill.side, fill.qty, fill.price);

                // Round-trip: position crossed through zero
                if (prev_pos > 0 && position_qty <= 0) || (prev_pos < 0 && position_qty >= 0) {
                    realized_pnl -= cost_basis; // cost_basis flips sign = realized
                    cost_basis = 0;
                }
            }
        }

        verifier.tick();
    }

    let hash = execution.state_hash();
    verifier.on_verified(next_seq, hash);

    (realized_pnl, fill_count, hash)
}

#[test]
fn e2e_full_pipeline_produces_fills() {
    let events = generate_synthetic_events(10_000);
    let (_pnl, fills, hash) = run_pipeline(&events);

    assert!(fills > 0, "Pipeline must produce at least one fill, got 0");
    assert!(hash != [0u8; 8], "State hash must be non-zero after processing");
    println!(
        "E2E: {} fills, PnL={}, hash={}",
        fills,
        _pnl,
        hex::encode(hash)
    );
}

#[test]
fn e2e_determinism_same_events_same_result() {
    let events = generate_synthetic_events(5_000);

    let (pnl_a, fills_a, hash_a) = run_pipeline(&events);
    let (pnl_b, fills_b, hash_b) = run_pipeline(&events);

    assert_eq!(pnl_a, pnl_b, "PnL must be deterministic");
    assert_eq!(fills_a, fills_b, "Fill count must be deterministic");
    assert_eq!(hash_a, hash_b, "State hash must be deterministic");
    println!(
        "Determinism OK: fills={}, hash={}",
        fills_a,
        hex::encode(hash_a)
    );
}

#[test]
fn e2e_risk_engine_tracks_consistently() {
    let mut hotpath = HotPathEngine::new(100_000, 20_000_000, 10, 1000, 3, 2, 5, true);
    let mut sim = SimulatedExchange::new(SimulatorConfig::default());
    let mut risk = RiskEngine::new(RiskLimits::default());
    let source = [0u8; 32];

    let events = generate_synthetic_events(1_000);
    let mut next_seq = 2000u64;
    let mut spread_updates = 0u64;

    for event in &events {
        if let EventPayload::OrderBookUpdate(book) = &event.payload {
            sim.on_book_update(&book.symbol, &book.bids, &book.asks, book.is_snapshot);
            if let (Some(b), Some(a)) = (book.bids.first(), book.asks.first()) {
                risk.update_spread(a.price.0 - b.price.0);
                spread_updates += 1;
            }
            let result = hotpath.on_book_update(&book.bids, &book.asks, book.is_snapshot);
            for i in 0..result.count as usize {
                let cmd = &result.commands[i];
                if cmd.tag == HpCmdTag::PlaceBid as i32 || cmd.tag == HpCmdTag::PlaceAsk as i32 {
                    let side = if cmd.tag == HpCmdTag::PlaceBid as i32 { Side::Buy } else { Side::Sell };
                    let order_id = OrderId::new(Venue::Simulated, next_seq);

                    // Pre-trade risk check
                    let check = risk.check_order(&sym(), side, Qty(cmd.qty), Price(cmd.price), None, event.timestamp);
                    if check.is_ok() {
                        let order_new = OrderNewEvent {
                            order_id,
                            symbol: sym(),
                            side,
                            order_type: OrderType::Limit,
                            price: Price(cmd.price),
                            qty: Qty(cmd.qty),
                            time_in_force: TimeInForce::GoodTilCancel,
                            venue: Venue::Simulated,
                        };
                        sim.submit_order(&order_new, event.timestamp, source);
                        risk.on_order_opened();
                        next_seq += 1;
                    }
                }
            }
        }

        let sim_events = sim.drain_events(next_seq);
        next_seq += sim_events.len() as u64;
        for sim_ev in &sim_events {
            if let EventPayload::OrderFill(fill) = &sim_ev.payload {
                risk.on_order_closed();
                risk.update_pnl(fill.price.0 * fill.qty.0 as i64 / 100_000_000);
            }
        }
    }

    assert!(spread_updates > 0, "Spread must be tracked");
    assert!(!risk.is_killed(), "Risk kill switch should not trip with default limits");
    println!(
        "Risk OK: spread_updates={}, pnl={}, circuit_breaker={}",
        spread_updates,
        risk.total_pnl(),
        risk.is_circuit_breaker()
    );
}

/// E2E: kill switch fires → all subsequent orders rejected, open orders cancellable.
#[test]
fn e2e_kill_switch_blocks_orders_and_cancels_open() {
    let mut hotpath = HotPathEngine::new(100_000, 20_000_000, 10, 1000, 3, 2, 5, true);
    let mut sim = SimulatedExchange::new(SimulatorConfig::default());
    let mut execution = ExecutionEngine::new();
    let mut risk = RiskEngine::new(RiskLimits::default());
    let source = [0u8; 32];

    let events = generate_synthetic_events(500);
    let mut next_seq = 5000u64;
    let mut orders_before_kill = 0u64;
    let mut orders_rejected_after_kill = 0u64;

    // Phase 1: run normally for 200 events to build some orders
    for event in &events[..200] {
        if let EventPayload::OrderBookUpdate(book) = &event.payload {
            sim.on_book_update(&book.symbol, &book.bids, &book.asks, book.is_snapshot);
            if let (Some(b), Some(a)) = (book.bids.first(), book.asks.first()) {
                risk.update_spread(a.price.0 - b.price.0);
            }
            let result = hotpath.on_book_update(&book.bids, &book.asks, book.is_snapshot);
            for i in 0..result.count as usize {
                let cmd = &result.commands[i];
                if cmd.tag == HpCmdTag::PlaceBid as i32 || cmd.tag == HpCmdTag::PlaceAsk as i32 {
                    let side = if cmd.tag == HpCmdTag::PlaceBid as i32 { Side::Buy } else { Side::Sell };
                    let order_id = execution.next_order_id(Venue::Simulated);
                    let check = risk.check_order(&sym(), side, Qty(cmd.qty), Price(cmd.price), None, event.timestamp);
                    if check.is_ok() {
                        let order_new = OrderNewEvent {
                            order_id: order_id.clone(),
                            symbol: sym(),
                            side,
                            order_type: OrderType::Limit,
                            price: Price(cmd.price),
                            qty: Qty(cmd.qty),
                            time_in_force: TimeInForce::GoodTilCancel,
                            venue: Venue::Simulated,
                        };
                        let order_event = Event::new(0, event.timestamp, source, EventPayload::OrderNew(order_new.clone()));
                        let _ = execution.process_event(&order_event);
                        sim.submit_order(&order_new, event.timestamp, source);
                        risk.on_order_opened();
                        orders_before_kill += 1;
                        next_seq += 1;
                    }
                }
            }
        }
        let sim_events = sim.drain_events(next_seq);
        next_seq += sim_events.len() as u64;
        for sim_ev in &sim_events {
            let _ = execution.process_event(sim_ev);
            if matches!(&sim_ev.payload, EventPayload::OrderFill(_)) {
                risk.on_order_closed();
            }
        }
    }

    // Phase 2: activate kill switch
    risk.activate_kill_switch("E2E test kill");
    assert!(risk.is_killed());

    // Cancel all open orders
    let open_before = execution.open_order_ids();
    for oid in &open_before {
        sim.cancel_order(oid, 999_999_999, source);
    }
    let cancel_events = sim.drain_events(next_seq);
    next_seq += cancel_events.len() as u64;
    for ev in &cancel_events {
        let _ = execution.process_event(ev);
        risk.on_order_closed();
    }

    // Phase 3: try to submit new orders after kill — all must be rejected
    for event in &events[200..400] {
        if let EventPayload::OrderBookUpdate(book) = &event.payload {
            let result = hotpath.on_book_update(&book.bids, &book.asks, book.is_snapshot);
            for i in 0..result.count as usize {
                let cmd = &result.commands[i];
                if cmd.tag == HpCmdTag::PlaceBid as i32 || cmd.tag == HpCmdTag::PlaceAsk as i32 {
                    let side = if cmd.tag == HpCmdTag::PlaceBid as i32 { Side::Buy } else { Side::Sell };
                    let check = risk.check_order(&sym(), side, Qty(cmd.qty), Price(cmd.price), None, event.timestamp);
                    assert!(check.is_err(), "All orders must be rejected when kill switch is active");
                    orders_rejected_after_kill += 1;
                }
            }
        }
    }

    assert!(orders_before_kill > 0, "Must have placed orders before kill");
    assert!(orders_rejected_after_kill > 0, "Must have attempted orders after kill");
    assert!(execution.open_order_ids().is_empty(), "No open orders after cancel-all");
    println!(
        "Kill switch OK: {} orders before, {} rejected after, open={}",
        orders_before_kill, orders_rejected_after_kill, execution.open_order_ids().len()
    );
}

/// E2E: stale order detection finds orders stuck in Ack state.
#[test]
fn e2e_stale_order_detection() {
    let mut execution = ExecutionEngine::new();

    // Create and ack an order at ts=1000
    let oid = execution.next_order_id(Venue::Simulated);
    let order = atomic_execution::LiveOrder::new(
        oid.clone(),
        sym(),
        Side::Buy,
        OrderType::Limit,
        Price(70_000_00),
        Qty(100_000),
        TimeInForce::GoodTilCancel,
        Venue::Simulated,
        1000,
    );
    execution.register_order(order).unwrap();
    execution.on_ack(&oid.0, "EX1".into(), 2000).unwrap();

    // Create a fresh order at ts=50_000
    let oid2 = execution.next_order_id(Venue::Simulated);
    let order2 = atomic_execution::LiveOrder::new(
        oid2.clone(),
        sym(),
        Side::Sell,
        OrderType::Limit,
        Price(70_100_00),
        Qty(100_000),
        TimeInForce::GoodTilCancel,
        Venue::Simulated,
        50_000,
    );
    execution.register_order(order2).unwrap();
    execution.on_ack(&oid2.0, "EX2".into(), 50_000).unwrap();

    // At now=60_000 with timeout=30_000: only oid (updated at 2000) is stale
    let stale = execution.stale_order_ids(60_000, 30_000);
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0], oid);
    println!("Stale detection OK: found {} stale order(s)", stale.len());
}
