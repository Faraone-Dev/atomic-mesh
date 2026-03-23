use criterion::{black_box, criterion_group, criterion_main, Criterion};

use atomic_core::types::{Level, Price, Qty, Side};
use atomic_hotpath::HotPathEngine;

fn make_book(depth: usize) -> (Vec<Level>, Vec<Level>) {
    let mid = 7_322_350_i64; // ~$73,223.50
    let bids: Vec<Level> = (0..depth)
        .map(|i| Level {
            price: Price(mid - 50 - (i as i64) * 100),
            qty: Qty(50_000_000 + (i as u64) * 5_000_000),
        })
        .collect();
    let asks: Vec<Level> = (0..depth)
        .map(|i| Level {
            price: Price(mid + 50 + (i as i64) * 100),
            qty: Qty(40_000_000 + (i as u64) * 4_000_000),
        })
        .collect();
    (bids, asks)
}

fn make_engine() -> HotPathEngine {
    HotPathEngine::new(
        100_000, 20_000_000, 10, 1000, 3, 2, 5, true,
    )
}

/// Steady-state benchmark: engine created ONCE, warmed up,
/// then measured in a hot loop (same as production).
fn bench_on_book_update(c: &mut Criterion) {
    let mut engine = make_engine();
    let (bids, asks) = make_book(20);
    // Warm cache + engine state
    for _ in 0..100 {
        engine.on_book_update(&bids, &asks, true);
    }

    c.bench_function("hp_on_book_update (20-deep, warm)", |b| {
        b.iter(|| black_box(engine.on_book_update(&bids, &asks, false)))
    });
}

fn bench_on_trade(c: &mut Criterion) {
    let mut engine = make_engine();
    let (bids, asks) = make_book(20);
    for _ in 0..100 {
        engine.on_book_update(&bids, &asks, true);
    }

    c.bench_function("hp_on_trade (warm)", |b| {
        b.iter(|| black_box(engine.on_trade(Side::Buy, Price(7_322_400), Qty(5_000_000))))
    });
}

fn bench_full_tick_cycle(c: &mut Criterion) {
    let mut engine = make_engine();
    let (bids, asks) = make_book(20);
    for _ in 0..100 {
        engine.on_book_update(&bids, &asks, true);
    }

    c.bench_function("full_tick_cycle (book+trade+fill, warm)", |b| {
        b.iter(|| {
            let r1 = engine.on_book_update(&bids, &asks, false);
            let r2 = engine.on_trade(Side::Buy, Price(7_322_400), Qty(5_000_000));
            engine.on_fill(Side::Buy, Qty(100_000), Price(7_322_300));
            black_box((r1, r2))
        })
    });
}

fn bench_book_update_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_depth_scaling");
    for depth in [5, 10, 20, 40] {
        let (bids, asks) = make_book(depth);
        let mut engine = make_engine();
        for _ in 0..100 {
            engine.on_book_update(&bids, &asks, true);
        }
        group.bench_function(
            criterion::BenchmarkId::new("warm", depth),
            |b| b.iter(|| black_box(engine.on_book_update(&bids, &asks, false))),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_on_book_update, bench_on_trade, bench_full_tick_cycle, bench_book_update_scaling);
criterion_main!(benches);
