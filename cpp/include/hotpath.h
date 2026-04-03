/*
 * hotpath.h — C-compatible FFI header for atomic-mesh HFT hot path
 *
 * All functions are designed for nanosecond-scale execution:
 *   • Zero heap allocation after init
 *   • Cache-line aligned data structures
 *   • SIMD-accelerated order book + microprice
 *   • Branch-prediction hints on fast path
 *   • Lock-free ring buffer for inter-thread comms
 */

#ifndef ATOMIC_HOTPATH_H
#define ATOMIC_HOTPATH_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ────────────────────────── constants ──────────────────────── */
#define HP_MAX_LEVELS    40   /* max depth levels per side       */
#define HP_RING_CAPACITY 4096 /* must be power of 2              */
#define HP_CACHELINE     64

/* ────────────────────────── types ──────────────────────────── */

/* A single price level: price in pipettes, qty in base units   */
typedef struct {
    int64_t  price;
    uint64_t qty;
} hp_level_t;

/* Side enum matching Rust Side                                 */
typedef enum {
    HP_SIDE_BUY  = 0,
    HP_SIDE_SELL = 1,
} hp_side_t;

/* Strategy command tag                                         */
typedef enum {
    HP_CMD_NONE       = 0,
    HP_CMD_PLACE_BID  = 1,
    HP_CMD_PLACE_ASK  = 2,
    HP_CMD_CANCEL_ALL = 3,
} hp_cmd_tag_t;

/* A single strategy command returned by the hot path           */
typedef struct {
    hp_cmd_tag_t tag;
    int64_t      price;    /* pipettes        */
    uint64_t     qty;      /* base units      */
} hp_command_t;

/* Result from the MM hot function                              */
typedef struct {
    hp_command_t commands[4];   /* max 4 commands per tick       */
    int32_t      count;        /* number of valid commands       */
    int64_t      fair_value;   /* computed microprice            */
    int64_t      vpin;         /* VPIN × 10000                  */
    int64_t      skew;         /* inventory skew in pipettes     */
    int64_t      half_spread;  /* dynamic spread in pipettes     */
    int64_t      compute_ns;   /* time spent in hot path (ns)    */
} hp_result_t;

/* ────────────────── opaque handle (created once) ─────────────── */
typedef struct hp_engine hp_engine_t;

/* ────────────────── lifecycle ────────────────────────────────── */

/*
 * Create a new hot-path engine. Call ONCE at startup.
 *   order_qty     : default order quantity (base units)
 *   max_inventory : maximum inventory (base units)
 *   half_spread_pipettes: half-spread in pipettes
 *   gamma         : risk aversion × 10000
 *   warmup_ticks  : ticks before first quote
 *   cooldown_ticks: min ticks between requotes
 *   requote_threshold: min price move to requote (pipettes)
 */
hp_engine_t* hp_engine_create(
    uint64_t order_qty,
    int64_t  max_inventory,
    int32_t  half_spread_pipettes,
    int32_t  gamma,
    int32_t  warmup_ticks,
    int32_t  cooldown_ticks,
    int64_t  requote_threshold,
    bool     vpin_enabled
);

void hp_engine_destroy(hp_engine_t* engine);

/* ────────────────── hot-path functions (ns-scale) ───────────── */

/*
 * Feed a book update into the engine.
 * bids/asks: arrays of hp_level_t sorted by price desc/asc.
 * is_snapshot: if true, replaces entire book.
 * Returns: the MM result with commands to execute.
 */
hp_result_t hp_on_book_update(
    hp_engine_t*      engine,
    const hp_level_t* bids,
    int32_t           bid_count,
    const hp_level_t* asks,
    int32_t           ask_count,
    bool              is_snapshot
);

/*
 * Feed a trade into the engine (updates toxicity only).
 * Returns: HP_CMD_CANCEL_ALL if flow becomes toxic.
 */
hp_result_t hp_on_trade(
    hp_engine_t* engine,
    hp_side_t    side,
    int64_t      price,
    uint64_t     qty
);

/*
 * Notify the engine of a fill (updates inventory).
 */
void hp_on_fill(
    hp_engine_t* engine,
    hp_side_t    side,
    uint64_t     qty,
    int64_t      price
);

/* ────────────────── query functions ──────────────────────────── */

int64_t  hp_best_bid(const hp_engine_t* engine);
int64_t  hp_best_ask(const hp_engine_t* engine);
int64_t  hp_mid_price(const hp_engine_t* engine);
int64_t  hp_spread(const hp_engine_t* engine);
int64_t  hp_imbalance(const hp_engine_t* engine);  /* × 10000    */
int64_t  hp_position(const hp_engine_t* engine);
int64_t  hp_realized_pnl(const hp_engine_t* engine);

/* ────────────────── ring buffer (SPSC lock-free) ────────────── */

typedef struct hp_ring hp_ring_t;

hp_ring_t* hp_ring_create(void);
void       hp_ring_destroy(hp_ring_t* ring);

/* Returns true if push succeeded (not full) */
bool hp_ring_push(hp_ring_t* ring, const hp_result_t* result);

/* Returns true if pop succeeded (not empty) */
bool hp_ring_pop(hp_ring_t* ring, hp_result_t* out);

int32_t hp_ring_size(const hp_ring_t* ring);

#ifdef __cplusplus
}
#endif

#endif /* ATOMIC_HOTPATH_H */
