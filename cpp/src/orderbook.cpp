/*
 * orderbook.cpp — Cache-aligned flat L2 order book
 *
 * Replaces BTreeMap with flat sorted arrays:
 *   • 40 levels × 16 bytes = 640 bytes per side → fits in L1 cache
 *   • Contiguous memory for SIMD-friendly traversal
 *   • Binary search for level updates
 *   • No heap allocation after construction
 */

#include "hotpath.h"
#include <cstring>
#include <algorithm>

/*
 * Internal flat book: two sorted arrays of levels.
 * bids sorted DESCENDING by price (best bid first)
 * asks sorted ASCENDING  by price (best ask first)
 */
struct alignas(HP_CACHELINE) FlatBook {
    hp_level_t bids[HP_MAX_LEVELS];
    hp_level_t asks[HP_MAX_LEVELS];
    int32_t    bid_count;
    int32_t    ask_count;
    int64_t    last_update_id;

    FlatBook() : bid_count(0), ask_count(0), last_update_id(0) {
        std::memset(bids, 0, sizeof(bids));
        std::memset(asks, 0, sizeof(asks));
    }

    /* Apply a full snapshot: just copy in the levels */
    void apply_snapshot(const hp_level_t* b, int32_t bc,
                        const hp_level_t* a, int32_t ac) {
        bid_count = (bc > HP_MAX_LEVELS) ? HP_MAX_LEVELS : bc;
        ask_count = (ac > HP_MAX_LEVELS) ? HP_MAX_LEVELS : ac;
        std::memcpy(bids, b, bid_count * sizeof(hp_level_t));
        std::memcpy(asks, a, ask_count * sizeof(hp_level_t));
    }

    /* Apply delta: update or insert levels, remove if qty==0 */
    void apply_delta_side(hp_level_t* levels, int32_t& count,
                          const hp_level_t* updates, int32_t update_count,
                          bool descending) {
        for (int32_t i = 0; i < update_count; ++i) {
            int64_t  p = updates[i].price;
            uint64_t q = updates[i].qty;

            /* Binary search for the price */
            int lo = 0, hi = count;
            int pos = -1;
            while (lo < hi) {
                int mid = (lo + hi) >> 1;
                if (levels[mid].price == p) { pos = mid; break; }
                if (descending ? (levels[mid].price > p) : (levels[mid].price < p))
                    lo = mid + 1;
                else
                    hi = mid;
            }

            if (pos >= 0) {
                /* Level exists */
                if (q == 0) {
                    /* Remove: shift left */
                    std::memmove(&levels[pos], &levels[pos + 1],
                                 (count - pos - 1) * sizeof(hp_level_t));
                    --count;
                } else {
                    levels[pos].qty = q;
                }
            } else if (q > 0 && count < HP_MAX_LEVELS) {
                /* Insert at lo */
                int insert_at = lo;
                std::memmove(&levels[insert_at + 1], &levels[insert_at],
                             (count - insert_at) * sizeof(hp_level_t));
                levels[insert_at].price = p;
                levels[insert_at].qty = q;
                ++count;
            }
        }
    }

    void apply_delta(const hp_level_t* b, int32_t bc,
                     const hp_level_t* a, int32_t ac) {
        apply_delta_side(bids, bid_count, b, bc, true);
        apply_delta_side(asks, ask_count, a, ac, false);
    }

    int64_t best_bid() const {
        return (bid_count > 0) ? bids[0].price : 0;
    }
    int64_t best_ask() const {
        return (ask_count > 0) ? asks[0].price : 0;
    }
    int64_t mid() const {
        if (bid_count == 0 || ask_count == 0) return 0;
        return (bids[0].price + asks[0].price) / 2;
    }
    int64_t spread() const {
        if (bid_count == 0 || ask_count == 0) return 0;
        return asks[0].price - bids[0].price;
    }
};
