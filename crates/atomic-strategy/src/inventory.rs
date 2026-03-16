use atomic_core::types::Side;

/// Inventory manager for Avellaneda-Stoikov market making.
///
/// Tracks position and computes quote skew so the MM naturally
/// offloads inventory: when long → tighten ask, widen bid.
///
/// Spot-safe: position never goes negative (can't short).
pub struct InventoryManager {
    /// Current position in base-unit satoshis (≥ 0 on spot).
    position_qty: i64,
    /// Maximum allowed inventory (satoshis).
    max_inventory: i64,
    /// Risk-aversion γ, scaled × 10 000 (e.g., 5 000 = 0.5).
    gamma: i64,
}

impl InventoryManager {
    pub fn new(max_inventory: i64, gamma: i64) -> Self {
        Self {
            position_qty: 0,
            max_inventory,
            gamma,
        }
    }

    /// Update position after a fill.
    pub fn on_fill(&mut self, side: Side, qty: u64) {
        match side {
            Side::Buy => self.position_qty += qty as i64,
            Side::Sell => self.position_qty -= qty as i64,
        }
        // Spot constraint
        if self.position_qty < 0 {
            self.position_qty = 0;
        }
    }

    /// Avellaneda-Stoikov quote skew (pipettes).
    ///
    /// `skew = γ × (q / q_max) × half_spread`
    ///
    /// Positive when long → lower ask (eager to sell), raise bid (less eager to buy).
    pub fn compute_skew(&self, half_spread: i64) -> i64 {
        if self.max_inventory == 0 {
            return 0;
        }
        // inv_ratio in [−10000, 10000]
        let inv_ratio = self.position_qty * 10000 / self.max_inventory;
        inv_ratio * self.gamma / 10000 * half_spread / 10000
    }

    /// Can we sell anything? (spot: need inventory > 0)
    pub fn can_sell(&self) -> bool {
        self.position_qty > 0
    }

    /// Max sellable quantity (capped by actual inventory).
    pub fn sellable_qty(&self, desired: u64) -> u64 {
        if self.position_qty <= 0 {
            return 0;
        }
        desired.min(self.position_qty as u64)
    }

    /// Are we at maximum inventory? (stop buying)
    pub fn at_max(&self) -> bool {
        self.position_qty >= self.max_inventory
    }

    /// Current position (satoshis).
    pub fn position(&self) -> i64 {
        self.position_qty
    }

    /// Force-set position (external sync / snapshot restore).
    pub fn set_position(&mut self, qty: i64) {
        self.position_qty = qty.max(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_buy_increases_position() {
        let mut inv = InventoryManager::new(100_000_000, 5000);
        inv.on_fill(Side::Buy, 10_000_000);
        assert_eq!(inv.position(), 10_000_000);
    }

    #[test]
    fn fill_sell_decreases_position() {
        let mut inv = InventoryManager::new(100_000_000, 5000);
        inv.on_fill(Side::Buy, 10_000_000);
        inv.on_fill(Side::Sell, 3_000_000);
        assert_eq!(inv.position(), 7_000_000);
    }

    #[test]
    fn spot_clamp_no_negative() {
        let mut inv = InventoryManager::new(100_000_000, 5000);
        inv.on_fill(Side::Sell, 10_000_000);
        assert_eq!(inv.position(), 0);
    }

    #[test]
    fn skew_flat_is_zero() {
        let inv = InventoryManager::new(100_000_000, 5000);
        assert_eq!(inv.compute_skew(7000), 0);
    }

    #[test]
    fn skew_long_is_positive() {
        let mut inv = InventoryManager::new(100_000_000, 5000);
        inv.on_fill(Side::Buy, 50_000_000); // 50% of max
        let skew = inv.compute_skew(7000);
        assert!(skew > 0, "long position should produce positive skew");
    }

    #[test]
    fn at_max_blocks_buying() {
        let mut inv = InventoryManager::new(100_000_000, 5000);
        inv.on_fill(Side::Buy, 100_000_000);
        assert!(inv.at_max());
        assert!(inv.can_sell());
    }

    #[test]
    fn sellable_qty_capped() {
        let mut inv = InventoryManager::new(100_000_000, 5000);
        inv.on_fill(Side::Buy, 5_000_000);
        assert_eq!(inv.sellable_qty(10_000_000), 5_000_000);
    }
}
