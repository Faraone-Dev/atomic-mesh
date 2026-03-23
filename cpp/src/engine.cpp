/*
 * engine.cpp — The unified hot-path engine
 *
 * Combines order book + microprice + toxicity + inventory + MM logic
 * into a single translation unit for maximum inlining.
 *
 * The hp_on_book_update() function is THE hot path:
 *   1. Apply book update (memcpy for snapshot, delta for incremental)
 *   2. Compute microprice (SIMD-ready flat array)
 *   3. Check requote conditions
 *   4. Compute dynamic spread (with toxicity multiplier)
 *   5. Compute inventory skew
 *   6. Generate bid/ask commands
 *
 * Total: ~50-200 ns depending on book depth and CPU.
 *
 * Compilation: g++ -O3 -march=native -std=c++17
 */

#include "hotpath.h"

/* Include the implementations directly for whole-program inlining */
#include "orderbook.cpp"
#include "microprice.cpp"
#include "signal.cpp"

#ifdef _WIN32
#include <windows.h>
static inline uint64_t rdtsc_ns() {
    /* Use QPC on Windows for reliable ns timing */
    LARGE_INTEGER freq, now;
    QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&now);
    return (uint64_t)((double)now.QuadPart * 1000000000.0 / (double)freq.QuadPart);
}
#else
#include <time.h>
static inline uint64_t rdtsc_ns() {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}
#endif

/* ────────────────── Engine State ──────────────────────────── */

struct hp_engine {
    /* Core components — each on its own cache line */
    FlatBook       book;
    ToxicityState  toxicity;
    InventoryState inventory;

    /* Strategy parameters (read-only after init) */
    alignas(HP_CACHELINE) struct {
        uint64_t order_qty;
        int32_t  half_spread_bps;
        int32_t  warmup_ticks;
        int32_t  cooldown_ticks;
        int64_t  requote_threshold;
        int32_t  microprice_depth;
        bool     vpin_enabled;
    } params;

    /* Mutable strategy state */
    alignas(HP_CACHELINE) struct {
        int64_t last_fair_value;
        int64_t last_bid_price;
        int64_t last_ask_price;
        int32_t tick_count;
        int32_t ticks_since_quote;
        bool    has_active_quotes;
    } state;

    hp_engine() {
        state.last_fair_value  = 0;
        state.last_bid_price   = 0;
        state.last_ask_price   = 0;
        state.tick_count       = 0;
        state.ticks_since_quote = 0;
        state.has_active_quotes = false;
        params.microprice_depth = 5;
    }
};

/* ────────────────── Requote Decision (branchless-optimized) ─ */

static inline bool should_requote(const hp_engine* e, int64_t fair_value) {
    /* First quote ever: always */
    if (e->state.last_fair_value == 0) {
        return true;
    }
    /* Warmup: not yet */
    if (e->state.tick_count < e->params.warmup_ticks) {
        return false;
    }
    /* Cooldown: too soon */
    if (e->state.ticks_since_quote < e->params.cooldown_ticks) {
        return false;
    }
    /* Price moved enough? */
    int64_t delta = fair_value - e->state.last_fair_value;
    if (delta < 0) delta = -delta;
    return delta >= e->params.requote_threshold;
}

/* ────────────────── Quote Generation ────────────────────────── */

static hp_result_t generate_quotes(hp_engine* e, int64_t fair_value) {
    hp_result_t result;
    result.count       = 0;
    result.fair_value  = fair_value;
    result.vpin        = e->toxicity.vpin();
    result.compute_ns  = 0;

    /* Fixed half-spread in pipettes (not bps) */
    int64_t base_hs = e->params.half_spread_bps;  /* direct pipettes */
    if (base_hs < 1) base_hs = 1;
    int64_t multiplier = e->toxicity.spread_multiplier();
    int64_t half_spread = base_hs * multiplier / 10000;

    result.half_spread = half_spread;

    /* Inventory skew */
    int64_t skew = e->inventory.compute_skew(half_spread);
    result.skew = skew;

    /* Cancel existing quotes first */
    if (e->state.has_active_quotes) {
        result.commands[result.count].tag   = HP_CMD_CANCEL_ALL;
        result.commands[result.count].price = 0;
        result.commands[result.count].qty   = 0;
        result.count++;
    }

    /* Maker-mode: passive quotes outside the fair value */
    /* Bid = FV − hs − skew (below mid, passive buy) */
    int64_t bid_price = fair_value - half_spread - skew;
    /* Ask = FV + hs − skew (above mid, passive sell) */
    int64_t ask_price = fair_value + half_spread - skew;

    /* Place bid (skip if at max long inventory) */
    if (!e->inventory.at_max_long()) {
        result.commands[result.count].tag   = HP_CMD_PLACE_BID;
        result.commands[result.count].price = bid_price;
        result.commands[result.count].qty   = e->params.order_qty;
        result.count++;
    }

    /* Place ask (skip if at max short inventory) */
    if (!e->inventory.at_max_short()) {
        result.commands[result.count].tag   = HP_CMD_PLACE_ASK;
        result.commands[result.count].price = ask_price;
        result.commands[result.count].qty   = e->params.order_qty;
        result.count++;
    }

    /* Update state */
    e->state.last_fair_value  = fair_value;
    e->state.last_bid_price   = bid_price;
    e->state.last_ask_price   = ask_price;
    e->state.ticks_since_quote = 0;
    e->state.has_active_quotes = true;

    return result;
}

/* ────────────────── Public C API ──────────────────────────── */

extern "C" {

hp_engine_t* hp_engine_create(
    uint64_t order_qty,
    int64_t  max_inventory,
    int32_t  half_spread_bps,
    int32_t  gamma,
    int32_t  warmup_ticks,
    int32_t  cooldown_ticks,
    int64_t  requote_threshold,
    bool     vpin_enabled
) {
    auto* e = new hp_engine();
    e->params.order_qty          = order_qty;
    e->params.half_spread_bps    = half_spread_bps;
    e->params.warmup_ticks       = warmup_ticks;
    e->params.cooldown_ticks     = cooldown_ticks;
    e->params.requote_threshold  = requote_threshold;
    e->params.microprice_depth   = 5;
    e->params.vpin_enabled       = vpin_enabled;
    e->inventory.max_inventory   = max_inventory;
    e->inventory.gamma           = gamma;
    e->toxicity.enabled          = vpin_enabled;
    return e;
}

void hp_engine_destroy(hp_engine_t* engine) {
    delete engine;
}

hp_result_t hp_on_book_update(
    hp_engine_t*      engine,
    const hp_level_t* bids,
    int32_t           bid_count,
    const hp_level_t* asks,
    int32_t           ask_count,
    bool              is_snapshot
) {
    uint64_t t0 = rdtsc_ns();

    /* 1. Apply book update */
    if (is_snapshot) {
        engine->book.apply_snapshot(bids, bid_count, asks, ask_count);
    } else {
        engine->book.apply_delta(bids, bid_count, asks, ask_count);
    }

    engine->state.tick_count++;
    engine->state.ticks_since_quote++;

    /* 2. Compute microprice */
    int64_t fv = hp_compute_microprice(
        engine->book.bids, engine->book.bid_count,
        engine->book.asks, engine->book.ask_count,
        engine->params.microprice_depth
    );

    if (fv == 0) {
        hp_result_t empty = {};
        empty.compute_ns = (int64_t)(rdtsc_ns() - t0);
        return empty;
    }

    /* 3. Requote check */
    if (!should_requote(engine, fv)) {
        hp_result_t noop = {};
        noop.fair_value  = fv;
        noop.vpin        = engine->toxicity.vpin();
        noop.compute_ns  = (int64_t)(rdtsc_ns() - t0);
        return noop;
    }

    /* 4. Generate quotes */
    hp_result_t result = generate_quotes(engine, fv);
    result.compute_ns = (int64_t)(rdtsc_ns() - t0);
    return result;
}

hp_result_t hp_on_trade(
    hp_engine_t* engine,
    hp_side_t    side,
    int64_t      price,
    uint64_t     qty
) {
    uint64_t t0 = rdtsc_ns();

    engine->toxicity.on_trade(side == HP_SIDE_BUY ? 1 : -1, qty, price);

    hp_result_t result = {};
    result.fair_value = engine->state.last_fair_value;
    result.vpin       = engine->toxicity.vpin();

    /* If flow just became toxic, cancel all */
    if (engine->toxicity.is_toxic() && engine->state.has_active_quotes) {
        result.commands[0].tag = HP_CMD_CANCEL_ALL;
        result.count = 1;
        engine->state.has_active_quotes = false;
    }

    result.compute_ns = (int64_t)(rdtsc_ns() - t0);
    return result;
}

void hp_on_fill(
    hp_engine_t* engine,
    hp_side_t    side,
    uint64_t     qty,
    int64_t      price
) {
    engine->inventory.on_fill(side == HP_SIDE_BUY ? 1 : -1, qty, price);
}

/* ────────────────── Query functions ──────────────────────── */

int64_t hp_best_bid(const hp_engine_t* e) { return e->book.best_bid(); }
int64_t hp_best_ask(const hp_engine_t* e) { return e->book.best_ask(); }
int64_t hp_mid_price(const hp_engine_t* e) { return e->book.mid(); }
int64_t hp_spread(const hp_engine_t* e)   { return e->book.spread(); }

int64_t hp_imbalance(const hp_engine_t* e) {
    return hp_compute_imbalance(
        e->book.bids, e->book.bid_count,
        e->book.asks, e->book.ask_count,
        5
    );
}

int64_t hp_position(const hp_engine_t* e) { return e->inventory.position_qty; }
int64_t hp_realized_pnl(const hp_engine_t* e) { return e->inventory.realized_pnl; }

} /* extern "C" */
