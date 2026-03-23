//! Multi-node determinism test: two independent pipeline instances
//! processing the same event stream must produce identical state hashes.
//!
//! This validates that the entire pipeline (C++ engine, simulator, execution)
//! is fully deterministic — a requirement for distributed mesh consensus.

use atomic_core::event::{Event, EventPayload, OrderBookUpdateEvent, TradeEvent};
use atomic_core::types::{Level, Price, Qty, Side, Symbol, Venue, OrderType, TimeInForce};
use atomic_core::event::OrderNewEvent;
use atomic_execution::{ExecutionEngine, SimulatedExchange, SimulatorConfig, StateVerifier};
use atomic_hotpath::{HotPathEngine, HpCmdTag};

fn sym() -> Symbol {
    Symbol::new("BTC", "USDT", Venue::Simulated)
}

/// Node: independent pipeline instance with its own engine + state.
struct TestNode {
    hotpath: HotPathEngine,
    sim: SimulatedExchange,
    execution: ExecutionEngine,
    verifier: StateVerifier,
    position_qty: i64,
    cost_basis: i64,
    realized_pnl: i64,
    fill_count: u64,
    next_seq: u64,
}

impl TestNode {
    fn new() -> Self {
        TestNode {
            hotpath: HotPathEngine::new(100_000, 20_000_000, 10, 1000, 3, 2, 5, true),
            sim: SimulatedExchange::new(SimulatorConfig::default()),
            execution: ExecutionEngine::new(),
            verifier: StateVerifier::new(1000),
            position_qty: 0,
            cost_basis: 0,
            realized_pnl: 0,
            fill_count: 0,
            next_seq: 100_000,
        }
    }

    fn process_event(&mut self, event: &Event) {
        let source = [0u8; 32];
        let result = match &event.payload {
            EventPayload::OrderBookUpdate(book) => {
                self.sim.on_book_update(&book.symbol, &book.bids, &book.asks, book.is_snapshot);
                self.hotpath.on_book_update(&book.bids, &book.asks, book.is_snapshot)
            }
            EventPayload::Trade(trade) => {
                self.hotpath.on_trade(trade.side, trade.price, trade.qty)
            }
            _ => return,
        };

        for i in 0..result.count as usize {
            let cmd = &result.commands[i];
            let tag = cmd.tag;
            if tag == HpCmdTag::CancelAll as i32 {
                for oid in self.execution.open_order_ids() {
                    self.sim.cancel_order(&oid, event.timestamp, source);
                }
            } else if tag == HpCmdTag::PlaceBid as i32 || tag == HpCmdTag::PlaceAsk as i32 {
                let side = if tag == HpCmdTag::PlaceBid as i32 { Side::Buy } else { Side::Sell };
                let order_id = self.execution.next_order_id(Venue::Simulated);
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
                self.sim.submit_order(&order_new, event.timestamp, source);
            }
        }

        let sim_events = self.sim.drain_events(self.next_seq);
        self.next_seq += sim_events.len() as u64;
        for sim_ev in &sim_events {
            let _ = self.execution.process_event(sim_ev);
            if let EventPayload::OrderFill(fill) = &sim_ev.payload {
                self.fill_count += 1;
                let signed_qty = if fill.side == Side::Buy { fill.qty.0 as i64 } else { -(fill.qty.0 as i64) };
                let prev_pos = self.position_qty;
                self.position_qty += signed_qty;
                self.cost_basis += signed_qty * fill.price.0;
                self.hotpath.on_fill(fill.side, fill.qty, fill.price);

                if (prev_pos > 0 && self.position_qty <= 0) || (prev_pos < 0 && self.position_qty >= 0) {
                    self.realized_pnl -= self.cost_basis;
                    self.cost_basis = 0;
                }
            }
        }

        self.verifier.tick();
    }

    fn finalize(&mut self) -> ([u8; 8], i64, u64) {
        let hash = self.execution.state_hash();
        self.verifier.on_verified(self.next_seq, hash);
        (hash, self.realized_pnl, self.fill_count)
    }
}

fn generate_events(count: usize) -> Vec<Event> {
    let source = [0u8; 32];
    let base_mid = 7_000_000_i64;
    let mut events = Vec::with_capacity(count * 2);
    let mut seq = 1u64;

    for i in 0..count {
        let drift = ((i as f64 * 0.01).sin() * 500.0) as i64;
        let mid = base_mid + drift;
        let spread_half = 50 + (i % 30) as i64;

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
            1_000_000_000 + (i as u64) * 100_000,
            source,
            EventPayload::OrderBookUpdate(OrderBookUpdateEvent {
                symbol: sym(),
                bids,
                asks,
                is_snapshot: i < 3,
                exchange_ts: 1_000_000_000 + (i as u64) * 100_000,
            }),
        ));
        seq += 1;

        if i % 3 == 0 {
            let side = if i % 6 == 0 { Side::Buy } else { Side::Sell };
            events.push(Event::new(
                seq,
                1_000_000_000 + (i as u64) * 100_000 + 50_000,
                source,
                EventPayload::Trade(TradeEvent {
                    symbol: sym(),
                    price: Price(mid),
                    qty: Qty(5_000_000),
                    side,
                    exchange_ts: 1_000_000_000 + (i as u64) * 100_000 + 50_000,
                }),
            ));
            seq += 1;
        }
    }
    events
}

#[test]
fn two_nodes_same_events_same_hash() {
    let events = generate_events(5_000);

    let mut node_a = TestNode::new();
    let mut node_b = TestNode::new();

    for event in &events {
        node_a.process_event(event);
        node_b.process_event(event);
    }

    let (hash_a, pnl_a, fills_a) = node_a.finalize();
    let (hash_b, pnl_b, fills_b) = node_b.finalize();

    assert_eq!(hash_a, hash_b, "Node A and B must produce identical state hashes");
    assert_eq!(pnl_a, pnl_b, "Node A and B must have identical PnL");
    assert_eq!(fills_a, fills_b, "Node A and B must have identical fill counts");
    assert!(fills_a > 0, "Nodes must produce fills");

    println!(
        "2-node determinism: fills={}, pnl={}, hash={}",
        fills_a,
        pnl_a,
        hex::encode(hash_a)
    );
}

#[test]
fn four_nodes_large_stream_deterministic() {
    let events = generate_events(10_000);

    let mut nodes: Vec<TestNode> = (0..4).map(|_| TestNode::new()).collect();

    for event in &events {
        for node in &mut nodes {
            node.process_event(event);
        }
    }

    let results: Vec<_> = nodes.iter_mut().map(|n| n.finalize()).collect();

    let (ref_hash, ref_pnl, ref_fills) = results[0];
    for (i, (hash, pnl, fills)) in results.iter().enumerate().skip(1) {
        assert_eq!(*hash, ref_hash, "Node {} hash diverged from node 0", i);
        assert_eq!(*pnl, ref_pnl, "Node {} PnL diverged from node 0", i);
        assert_eq!(*fills, ref_fills, "Node {} fills diverged from node 0", i);
    }

    assert!(ref_fills > 0, "Nodes must produce fills");
    println!(
        "4-node determinism: fills={}, pnl={}, hash={}",
        ref_fills,
        ref_pnl,
        hex::encode(ref_hash)
    );
}
