# ATOMIC MESH — Backtest Report

**Data**: `data/backtest_10k.log` — 13,334 synthetic BTCUSDT events (10k book ticks + 3,334 trades)
**Engine**: C++ HotPath (Avellaneda-Stoikov MM)
**Date**: 2026-03-22

## Results

| Metric | Value |
|--------|-------|
| Events processed | 13,334 (249k evt/s) |
| Orders | 196 |
| Fills | 196 |
| Round-trips | 98 |
| Volume | $1,428,137.00 |
| Realized PnL | +$15.68 |
| Max Drawdown | $0.00 |
| Win Rate | 100.0% |
| Avg Trade PnL | +$0.16 |
| Sharpe (trade) | 999.999 |
| Final Position | 0 sats (flat) |

## Engine Parameters

| Parameter | Value |
|-----------|-------|
| order_qty | 10,000,000 sats (0.1 BTC) |
| max_inventory | 10,000,000 sats (0.1 BTC) |
| half_spread | 80 pipettes ($0.80) |
| gamma | 5000 (0.5) |
| warmup_ticks | 3 |
| cooldown_ticks | 5 |
| requote_threshold | 200 |
| vpin_enabled | false |

## Notes

- Synthetic data generated with deterministic PRNG (fully reproducible via `cargo run --release --bin generate-backtest-data`)
- Price oscillates around $73,223.50 with mean-reverting Brownian motion
- Symmetric bid/ask quantities for unbiased microprice
- 20-deep order book per side with all-snapshot updates
- Trades every 3rd tick with balanced buy/sell distribution
- SimulatedExchange: passive maker fill at limit price, 0bps maker (Binance VIP/MM tier) / 5bps taker
- Position-aware command gating: engine only submits new orders when flat, preventing spread destruction from requotes
- 98 round-trips with 100% win rate — every completed round-trip captured the full bid-ask spread
- 11 bugs fixed in C++/Rust pipeline to achieve positive PnL (see commit `f349605`)
- Equity curve exported to `results/equity.csv`
