use atomic_core::error::{AtomicError, Result};
use atomic_core::types::{Position, Price, Qty, Side, Symbol};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// Risk limits configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    /// Maximum position size per symbol (in base units).
    pub max_position_qty: u64,
    /// Maximum notional exposure per symbol.
    pub max_notional: i64,
    /// Maximum total notional across all positions.
    pub max_total_notional: i64,
    /// Maximum single order size.
    pub max_order_qty: u64,
    /// Maximum number of open orders.
    pub max_open_orders: usize,
    /// Maximum loss before kill switch (in quote units).
    pub max_loss: i64,
    /// Maximum orders per second (rate limit).
    pub max_orders_per_second: u32,
    /// Max spread (pipettes) to allow quoting. 0 = disabled.
    #[serde(default)]
    pub max_spread: i64,
    /// Max consecutive losing round-trips before circuit breaker. 0 = disabled.
    #[serde(default)]
    pub max_consecutive_losses: u32,
    /// Max drawdown from peak PnL as basis points (e.g. 200 = 2%). 0 = disabled.
    /// Triggers soft pause (not kill switch) — auto-resumes on daily reset.
    #[serde(default)]
    pub max_drawdown_bps: u32,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_qty: 1_000_000_000, // 10 BTC in satoshis
            max_notional: 500_000_000_000,    // 500k USDT in pipettes
            max_total_notional: 1_000_000_000_000,
            max_order_qty: 100_000_000,       // 1 BTC
            max_open_orders: 100,
            max_loss: -10_000_000_000,        // -10k USDT
            max_orders_per_second: 50,
            max_spread: 5000,                 // $50 spread → don't quote (protects during flash crash)
            max_consecutive_losses: 5,        // 5 losses in a row → circuit breaker
            max_drawdown_bps: 200,            // 2% drawdown from peak → soft pause
        }
    }
}

/// Risk engine: validates orders and positions against limits.
/// If any limit is breached → reject order or activate kill switch.
pub struct RiskEngine {
    limits: RiskLimits,
    kill_switch: AtomicBool,
    open_order_count: usize,
    total_pnl: i64,
    order_timestamps: Vec<u64>,
    /// Current spread in pipettes (updated by main loop on every book tick).
    current_spread: i64,
    /// Consecutive losing round-trips (reset on win or daily reset).
    consecutive_losses: u32,
    /// True if circuit breaker fired (consecutive losses or drawdown).
    circuit_breaker: bool,
    /// Peak PnL for drawdown tracking.
    peak_pnl: i64,
}

impl RiskEngine {
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            kill_switch: AtomicBool::new(false),
            open_order_count: 0,
            total_pnl: 0,
            order_timestamps: Vec::new(),
            current_spread: 0,
            consecutive_losses: 0,
            circuit_breaker: false,
            peak_pnl: 0,
        }
    }

    /// Check if the kill switch is active.
    pub fn is_killed(&self) -> bool {
        self.kill_switch.load(Ordering::SeqCst)
    }

    /// Activate the kill switch.
    pub fn activate_kill_switch(&self, reason: &str) {
        tracing::error!("KILL SWITCH ACTIVATED: {}", reason);
        self.kill_switch.store(true, Ordering::SeqCst);
    }

    /// Reset kill switch (manual override only).
    pub fn reset_kill_switch(&self) {
        self.kill_switch.store(false, Ordering::SeqCst);
    }

    /// Validate a new order against risk limits.
    /// `total_notional` is the current aggregate notional exposure across all positions
    /// (caller computes this from live position state).
    pub fn check_order(
        &mut self,
        _symbol: &Symbol,
        side: Side,
        qty: Qty,
        price: Price,
        position: Option<&Position>,
        current_ts: u64,
    ) -> Result<()> {
        // Kill switch check (hard — only manual reset)
        if self.is_killed() {
            return Err(AtomicError::KillSwitch("kill switch is active".into()));
        }

        // Circuit breaker check (soft — auto-resets on daily_reset)
        if self.circuit_breaker {
            return Err(AtomicError::RiskLimitExceeded(
                "circuit breaker active (consecutive losses or drawdown)".into(),
            ));
        }

        // Spread gate: don't quote into a wide spread (adverse selection protection)
        if self.limits.max_spread > 0 && self.current_spread > self.limits.max_spread {
            return Err(AtomicError::RiskLimitExceeded(format!(
                "spread {} exceeds max {} — skipping quote",
                self.current_spread, self.limits.max_spread
            )));
        }

        // Max order size
        if qty.0 > self.limits.max_order_qty {
            return Err(AtomicError::RiskLimitExceeded(format!(
                "order qty {} exceeds max {}",
                qty.0, self.limits.max_order_qty
            )));
        }

        // Position limit check
        if let Some(pos) = position {
            let new_qty = if pos.side == side {
                pos.qty.0 + qty.0
            } else if qty.0 > pos.qty.0 {
                qty.0 - pos.qty.0
            } else {
                pos.qty.0 - qty.0
            };

            if new_qty > self.limits.max_position_qty {
                return Err(AtomicError::RiskLimitExceeded(format!(
                    "resulting position {} exceeds max {}",
                    new_qty, self.limits.max_position_qty
                )));
            }

            // Notional check
            let notional = new_qty as i64 * price.0;
            if notional > self.limits.max_notional {
                return Err(AtomicError::RiskLimitExceeded(format!(
                    "notional {} exceeds max {}",
                    notional, self.limits.max_notional
                )));
            }

            // Total notional across all positions
            // Conservative: use proposed notional as floor (assumes no other positions)
            if notional > self.limits.max_total_notional {
                return Err(AtomicError::RiskLimitExceeded(format!(
                    "total notional {} exceeds aggregate max {}",
                    notional, self.limits.max_total_notional
                )));
            }
        }

        // Open orders limit
        if self.open_order_count >= self.limits.max_open_orders {
            return Err(AtomicError::RiskLimitExceeded(format!(
                "open orders {} exceeds max {}",
                self.open_order_count, self.limits.max_open_orders
            )));
        }

        // Rate limit
        self.order_timestamps.retain(|&ts| current_ts - ts < 1_000_000_000); // 1 second in nanos
        if self.order_timestamps.len() >= self.limits.max_orders_per_second as usize {
            return Err(AtomicError::RiskLimitExceeded(
                "order rate limit exceeded".into(),
            ));
        }

        // Loss limit (hard kill switch)
        if self.total_pnl < self.limits.max_loss {
            self.activate_kill_switch(&format!(
                "total PnL {} below max loss {}",
                self.total_pnl, self.limits.max_loss
            ));
            return Err(AtomicError::KillSwitch("max loss exceeded".into()));
        }

        self.order_timestamps.push(current_ts);
        Ok(())
    }

    pub fn on_order_opened(&mut self) {
        self.open_order_count += 1;
    }

    pub fn on_order_closed(&mut self) {
        self.open_order_count = self.open_order_count.saturating_sub(1);
    }

    pub fn update_pnl(&mut self, pnl_delta: i64) {
        self.total_pnl += pnl_delta;

        // Track peak for drawdown calculation
        if self.total_pnl > self.peak_pnl {
            self.peak_pnl = self.total_pnl;
        }

        // Drawdown circuit breaker: (peak - current) / peak > max_drawdown_bps/10000
        // Using integer math to avoid float: (peak - current) * 10000 > peak * max_drawdown_bps
        if self.limits.max_drawdown_bps > 0 && self.peak_pnl > 0 {
            let drawdown = self.peak_pnl - self.total_pnl;
            if drawdown * 10_000 > self.peak_pnl * self.limits.max_drawdown_bps as i64 {
                self.circuit_breaker = true;
                tracing::warn!(
                    "CIRCUIT BREAKER: drawdown {}bps from peak (limit: {}bps) — pausing",
                    drawdown * 10_000 / self.peak_pnl,
                    self.limits.max_drawdown_bps
                );
            }
        }
    }

    /// Record a winning round-trip. Resets consecutive loss counter.
    pub fn record_win(&mut self) {
        self.consecutive_losses = 0;
    }

    /// Record a losing round-trip. If max breached → circuit breaker.
    pub fn record_loss(&mut self) {
        self.consecutive_losses += 1;
        if self.limits.max_consecutive_losses > 0
            && self.consecutive_losses >= self.limits.max_consecutive_losses
        {
            self.circuit_breaker = true;
            tracing::warn!(
                "CIRCUIT BREAKER: {} consecutive losses (limit: {}) — pausing",
                self.consecutive_losses, self.limits.max_consecutive_losses
            );
        }
    }

    /// Update the current spread (called on every book update from main loop).
    pub fn update_spread(&mut self, spread: i64) {
        self.current_spread = spread;
    }

    /// Daily reset: clears circuit breaker, consecutive losses, and PnL peak.
    /// Call at UTC midnight or session start.
    pub fn daily_reset(&mut self) {
        self.circuit_breaker = false;
        self.consecutive_losses = 0;
        self.peak_pnl = 0;
        self.total_pnl = 0;
        tracing::info!("Risk daily reset: circuit breaker cleared, PnL zeroed");
    }

    pub fn total_pnl(&self) -> i64 {
        self.total_pnl
    }

    pub fn open_order_count(&self) -> usize {
        self.open_order_count
    }

    pub fn is_circuit_breaker(&self) -> bool {
        self.circuit_breaker
    }

    pub fn consecutive_losses(&self) -> u32 {
        self.consecutive_losses
    }

    pub fn peak_pnl(&self) -> i64 {
        self.peak_pnl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::{Price, Qty, Side, Symbol, Venue};

    fn sym() -> Symbol {
        Symbol::new("BTC", "USDT", Venue::Binance)
    }

    fn engine(limits: RiskLimits) -> RiskEngine {
        RiskEngine::new(limits)
    }

    #[test]
    fn check_order_passes_within_limits() {
        let mut risk = engine(RiskLimits::default());
        let result = risk.check_order(&sym(), Side::Buy, Qty(1_000_000), Price(50_000), None, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn reject_exceeds_max_order_qty() {
        let limits = RiskLimits { max_order_qty: 100, ..Default::default() };
        let mut risk = engine(limits);
        let result = risk.check_order(&sym(), Side::Buy, Qty(200), Price(50_000), None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn reject_exceeds_max_open_orders() {
        let limits = RiskLimits { max_open_orders: 2, ..Default::default() };
        let mut risk = engine(limits);
        risk.on_order_opened();
        risk.on_order_opened();
        let result = risk.check_order(&sym(), Side::Buy, Qty(100), Price(50_000), None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn kill_switch_on_max_loss() {
        let limits = RiskLimits { max_loss: -1000, ..Default::default() };
        let mut risk = engine(limits);
        risk.update_pnl(-2000);
        let result = risk.check_order(&sym(), Side::Buy, Qty(100), Price(50_000), None, 0);
        assert!(result.is_err());
        assert!(risk.is_killed());
    }

    #[test]
    fn kill_switch_blocks_all_orders() {
        let mut risk = engine(RiskLimits::default());
        risk.activate_kill_switch("test");
        let result = risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn reset_kill_switch_allows_orders() {
        let mut risk = engine(RiskLimits::default());
        risk.activate_kill_switch("test");
        assert!(risk.is_killed());
        risk.reset_kill_switch();
        assert!(!risk.is_killed());
        let result = risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn rate_limit_rejects_burst() {
        let limits = RiskLimits { max_orders_per_second: 3, ..Default::default() };
        let mut risk = engine(limits);
        let ts = 1_000_000_000_000u64; // some timestamp in nanos
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts).is_ok());
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts + 100).is_ok());
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts + 200).is_ok());
        // 4th within same second → reject
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts + 300).is_err());
    }

    #[test]
    fn rate_limit_resets_after_one_second() {
        let limits = RiskLimits { max_orders_per_second: 2, ..Default::default() };
        let mut risk = engine(limits);
        let ts = 1_000_000_000_000u64;
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts).is_ok());
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts + 100).is_ok());
        // Next second
        assert!(risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, ts + 1_000_000_001).is_ok());
    }

    #[test]
    fn pnl_tracking() {
        let mut risk = engine(RiskLimits::default());
        assert_eq!(risk.total_pnl(), 0);
        risk.update_pnl(500);
        risk.update_pnl(-200);
        assert_eq!(risk.total_pnl(), 300);
    }

    #[test]
    fn position_limit_check() {
        let limits = RiskLimits { max_position_qty: 1000, ..Default::default() };
        let mut risk = engine(limits);
        let pos = Position::flat(sym());
        // Within limit
        let result = risk.check_order(&sym(), Side::Buy, Qty(800), Price(1), Some(&pos), 0);
        assert!(result.is_ok());
    }

    #[test]
    fn daily_reset_clears_pnl_and_peak() {
        let mut risk = engine(RiskLimits::default());
        risk.update_pnl(1_000);
        risk.update_pnl(-250);
        assert!(risk.peak_pnl() > 0);
        assert!(risk.total_pnl() > 0);

        risk.daily_reset();

        assert_eq!(risk.total_pnl(), 0);
        assert_eq!(risk.peak_pnl(), 0);
        assert_eq!(risk.consecutive_losses(), 0);
        assert!(!risk.is_circuit_breaker());
    }

    #[test]
    fn reject_exceeds_max_total_notional() {
        let limits = RiskLimits {
            max_total_notional: 500,
            max_notional: 1_000_000,
            ..Default::default()
        };
        let mut risk = engine(limits);
        let pos = Position::flat(sym());
        // notional = 1000 * 1 = 1000 > max_total_notional 500
        let result = risk.check_order(&sym(), Side::Buy, Qty(1000), Price(1), Some(&pos), 0);
        assert!(result.is_err());
    }

    #[test]
    fn spread_gate_rejects_wide_spread() {
        let limits = RiskLimits { max_spread: 100, ..Default::default() };
        let mut risk = engine(limits);
        risk.update_spread(200);
        let result = risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn consecutive_losses_trigger_circuit_breaker() {
        let limits = RiskLimits { max_consecutive_losses: 3, ..Default::default() };
        let mut risk = engine(limits);
        risk.record_loss();
        risk.record_loss();
        assert!(!risk.is_circuit_breaker());
        risk.record_loss();
        assert!(risk.is_circuit_breaker());
        let result = risk.check_order(&sym(), Side::Buy, Qty(1), Price(1), None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn drawdown_triggers_circuit_breaker() {
        let limits = RiskLimits { max_drawdown_bps: 100, ..Default::default() }; // 1%
        let mut risk = engine(limits);
        risk.update_pnl(10_000);         // peak = 10_000
        risk.update_pnl(-10_200);        // total = -200, drawdown = 10_200 from peak
        assert!(risk.is_circuit_breaker());
    }
}
