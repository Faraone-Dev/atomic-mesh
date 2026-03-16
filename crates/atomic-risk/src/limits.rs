use atomic_core::error::{AtomicError, Result};
use atomic_core::types::{Position, Price, Qty, Side, Symbol, Venue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
}

impl RiskEngine {
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            kill_switch: AtomicBool::new(false),
            open_order_count: 0,
            total_pnl: 0,
            order_timestamps: Vec::new(),
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
    pub fn check_order(
        &mut self,
        symbol: &Symbol,
        side: Side,
        qty: Qty,
        price: Price,
        position: Option<&Position>,
        current_ts: u64,
    ) -> Result<()> {
        // Kill switch check
        if self.is_killed() {
            return Err(AtomicError::KillSwitch("kill switch is active".into()));
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

        // Loss limit
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
    }

    pub fn total_pnl(&self) -> i64 {
        self.total_pnl
    }
}
