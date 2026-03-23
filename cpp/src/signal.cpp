/*
 * signal.cpp — VPIN toxicity + volatility + inventory skew
 *
 * All signal processing in one file to maximize inlining.
 * No heap allocation — uses fixed-size circular buffers.
 */

#include "hotpath.h"
#include <cstring>
#include <cstdlib>

#define VPIN_WINDOW     200    /* rolling window size            */
#define VOL_ALPHA       500    /* EMA alpha × 10000              */
#define TOXIC_THRESHOLD 8000   /* VPIN > 0.80 = toxic            */

/* ────────────────── Toxicity Tracker ──────────────────────── */

struct alignas(HP_CACHELINE) ToxicityState {
    /* VPIN circular buffer */
    int32_t  trade_sides[VPIN_WINDOW]; /* +1 = buy, -1 = sell      */
    uint64_t trade_qtys[VPIN_WINDOW];
    int32_t  head;                      /* next write position       */
    int32_t  count;                     /* filled entries             */

    /* Running sums for O(1) VPIN */
    int64_t  buy_volume;
    int64_t  sell_volume;
    uint64_t total_volume;

    /* Volatility EMA */
    int64_t  last_price;
    int64_t  vol_ema;                   /* × 10000                   */

    /* Runtime flag: if false, is_toxic()/spread_multiplier() are no-ops */
    bool     enabled;

    ToxicityState() {
        std::memset(this, 0, sizeof(*this));
        enabled = false;
    }

    void on_trade(int32_t side, uint64_t qty, int64_t price) {
        /* Evict oldest if buffer full */
        if (count >= VPIN_WINDOW) {
            int old_idx = head;
            if (trade_sides[old_idx] > 0) {
                buy_volume -= (int64_t)trade_qtys[old_idx];
            } else {
                sell_volume -= (int64_t)trade_qtys[old_idx];
            }
            total_volume -= trade_qtys[old_idx];
        }

        /* Insert new trade */
        trade_sides[head] = side;
        trade_qtys[head]  = qty;

        if (side > 0) {
            buy_volume += (int64_t)qty;
        } else {
            sell_volume += (int64_t)qty;
        }
        total_volume += qty;

        head = (head + 1) % VPIN_WINDOW;
        if (count < VPIN_WINDOW) ++count;

        /* Update volatility EMA: |price - last_price| */
        if (last_price != 0) {
            int64_t delta = price - last_price;
            if (delta < 0) delta = -delta;
            /* EMA: vol = alpha * delta + (1-alpha) * vol */
            vol_ema = (VOL_ALPHA * delta + (10000 - VOL_ALPHA) * vol_ema) / 10000;
        }
        last_price = price;
    }

    /* VPIN: |buy_vol - sell_vol| / total_vol × 10000 */
    int64_t vpin() const {
        if (total_volume == 0) return 0;
        int64_t diff = buy_volume - sell_volume;
        if (diff < 0) diff = -diff;
        return diff * 10000 / (int64_t)total_volume;
    }

    bool is_toxic() const {
        if (!enabled) return false;
        return vpin() > TOXIC_THRESHOLD;
    }

    /* Spread multiplier: widen spread when toxic */
    int64_t spread_multiplier() const {
        if (!enabled) return 10000;
        int64_t v = vpin();
        if (v > TOXIC_THRESHOLD) {
            /* Scale: 1× at threshold, up to 2× at VPIN=10000 */
            return 10000 + (v - TOXIC_THRESHOLD) * 10000 / (10000 - TOXIC_THRESHOLD);
        }
        return 10000;
    }
};

/* ────────────────── Inventory Manager ─────────────────────── */

struct alignas(HP_CACHELINE) InventoryState {
    int64_t position_qty;    /* signed: positive = long         */
    int64_t max_inventory;   /* absolute max                    */
    int64_t gamma;           /* risk aversion × 10000           */
    int64_t cost_basis;      /* avg entry price (pipettes)      */
    int64_t realized_pnl;    /* cumulative P&L (pipettes)       */

    InventoryState() : position_qty(0), max_inventory(0), gamma(0),
                       cost_basis(0), realized_pnl(0) {}

    void on_fill(int32_t side, uint64_t qty, int64_t price) {
        int64_t signed_qty = (int64_t)qty;
        if (side > 0) {
            /* Buy (side=1 from hp_on_fill): increase position */
            position_qty += signed_qty;
        } else {
            /* Sell (side=-1): decrease position (can go short) */
            position_qty -= signed_qty;
        }
        /* Clamp to ±max_inventory */
        if (position_qty > max_inventory) position_qty = max_inventory;
        if (position_qty < -max_inventory) position_qty = -max_inventory;
    }

    /*
     * Inventory skew (Avellaneda-Stoikov):
     *   skew = gamma × position / max_inventory × half_spread
     * Returns pipettes of price adjustment.
     */
    int64_t compute_skew(int64_t half_spread) const {
        if (max_inventory == 0) return 0;
        /* skew = gamma/10000 * (pos/max_inv) * hs
         *      = gamma * pos * hs / (10000 * max_inv)  */
        return gamma * position_qty * half_spread / (10000 * max_inventory);
    }

    bool at_max_long() const {
        return position_qty >= max_inventory;
    }

    bool at_max_short() const {
        return position_qty <= -max_inventory;
    }
};
