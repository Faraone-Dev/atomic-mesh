//! Generates a realistic synthetic BTCUSDT dataset for backtesting.
//! Outputs to data/backtest_10k.log — deterministic, reproducible.
//!
//! Run: cargo run --release --bin generate-backtest-data

use atomic_core::event::{Event, EventPayload, OrderBookUpdateEvent, TradeEvent};
use atomic_core::types::{Level, Price, Qty, Side, Symbol, Venue};
use atomic_replay::EventLog;

fn sym() -> Symbol {
    Symbol::new("BTC", "USDT", Venue::Binance)
}

fn main() {
    let path = "data/backtest_10k.log";
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate existing file before writing
    let _ = std::fs::remove_file(path);

    let mut log = EventLog::open(path).expect("Failed to create event log");
    let source = [0u8; 32]; // node-0

    let base_mid = 7_322_350_i64; // $73,223.50
    let mut seq = 1u64;
    let base_ts = 1_711_200_000_000_000_000_u64; // ~2024-03-23 in nanos
    let tick_interval = 100_000_000u64; // 100ms between ticks

    // Simulate 10,000 ticks of realistic BTC order book
    // Price oscillates with mean-reversion — ideal for market making
    let mut mid = base_mid as f64;

    for i in 0..10_000_usize {
        // Deterministic PRNG
        let seed = (i as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let raw = ((seed >> 33) ^ seed).wrapping_mul(0xff51afd7ed558ccd);
        let uniform = (raw as f64) / (u64::MAX as f64);
        let noise = (uniform - 0.5) * 2.0; // [-1, 1)

        // Primary oscillation (~80 tick period ≈ 8s): ±$5 swings
        let wave = ((i as f64) * 0.0785).sin() * 500.0;
        // Secondary slower oscillation (~400 tick period): ±$3 drift
        let drift = ((i as f64) * 0.0157).sin() * 300.0;
        // Small per-tick noise ±$0.20
        let jitter = noise * 20.0;

        // Mean-reversion toward base
        let reversion = (base_mid as f64 - mid) * 0.05;
        mid = base_mid as f64 + wave + drift + jitter + reversion;

        // Tight spread (5 pipettes = $0.05 — realistic for BTC on liquid venue)
        let spread_half: i64 = 5;

        let mid_int = mid as i64;

        // Build 20-deep book — symmetric quantities so microprice ≈ mid
        let bids: Vec<Level> = (0..20)
            .map(|d| {
                let depth_seed = seed.wrapping_add(d as u64 * 17);
                let qty_noise = ((depth_seed >> 40) % 20) as u64;
                Level {
                    price: Price(mid_int - spread_half - d * 100),
                    qty: Qty(10_000_000 + d as u64 * 5_000_000 + qty_noise * 1_000_000),
                }
            })
            .collect();
        let asks: Vec<Level> = (0..20)
            .map(|d| {
                let depth_seed = seed.wrapping_add(d as u64 * 17); // same seed as bids → symmetric microprice
                let qty_noise = ((depth_seed >> 40) % 20) as u64;
                Level {
                    price: Price(mid_int + spread_half + d * 100),
                    qty: Qty(10_000_000 + d as u64 * 5_000_000 + qty_noise * 1_000_000),
                }
            })
            .collect();

        let ts = base_ts + (i as u64) * tick_interval;

        // Book update
        let event = Event::new(
            seq,
            ts,
            source,
            EventPayload::OrderBookUpdate(OrderBookUpdateEvent {
                symbol: sym(),
                bids,
                asks,
                is_snapshot: true,  // always snapshot — clean sim book each tick
                exchange_ts: ts,
            }),
        );
        log.append(&event).unwrap();
        seq += 1;

        // Trade every ~3 ticks (aggressive orders hitting the book)
        if i % 3 == 0 {
            let trade_seed = seed.wrapping_mul(0x9e3779b97f4a7c15);
            let is_buy = (trade_seed >> 63) == 0;
            let aggression = ((trade_seed >> 50) % 5) as i64; // 0-4 ticks into book
            let trade_price = if is_buy {
                mid_int + spread_half + aggression * 100
            } else {
                mid_int - spread_half - aggression * 100
            };
            let trade_qty = 1_000_000 + ((trade_seed >> 32) % 20) as u64 * 500_000;

            let trade_event = Event::new(
                seq,
                ts + 50_000_000, // 50ms after book update
                source,
                EventPayload::Trade(TradeEvent {
                    symbol: sym(),
                    price: Price(trade_price),
                    qty: Qty(trade_qty),
                    side: if is_buy { Side::Buy } else { Side::Sell },
                    exchange_ts: ts + 50_000_000,
                }),
            );
            log.append(&trade_event).unwrap();
            seq += 1;
        }
    }

    log.flush().unwrap();
    println!("Generated {} events → {}", seq - 1, path);
    println!("Mid price range: ~${:.2}", mid / 100.0);
}
