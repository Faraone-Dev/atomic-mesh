/*
 * ringbuf.cpp — Lock-free SPSC ring buffer
 *
 * Single-producer, single-consumer queue for passing hp_result_t
 * between the hot-path thread and the Rust async runtime.
 *
 * Implementation:
 *   • Power-of-2 capacity with masking (no modulo)
 *   • Cache-line separated head/tail to avoid false sharing
 *   • std::memory_order_acquire/release for correct cross-thread visibility
 *   • No CAS loops — SPSC guarantees single writer per pointer
 */

#include "hotpath.h"
#include <atomic>
#include <cstring>
#include <new>
#ifdef _WIN32
#include <malloc.h>
#endif

struct hp_ring {
    /* head and tail on separate cache lines to prevent false sharing */
    alignas(HP_CACHELINE) std::atomic<uint32_t> head{0};   /* written by producer */
    alignas(HP_CACHELINE) std::atomic<uint32_t> tail{0};   /* written by consumer */
    alignas(HP_CACHELINE) hp_result_t slots[HP_RING_CAPACITY];

    hp_ring() {
        std::memset(slots, 0, sizeof(slots));
    }
};

static_assert((HP_RING_CAPACITY & (HP_RING_CAPACITY - 1)) == 0,
              "Ring capacity must be power of 2");

extern "C" {

hp_ring_t* hp_ring_create(void) {
    /* Use aligned allocation for cache-line friendliness */
#ifdef _WIN32
    void* mem = _aligned_malloc(sizeof(hp_ring), HP_CACHELINE);
#else
    void* mem = ::operator new(sizeof(hp_ring), std::align_val_t{HP_CACHELINE});
#endif
    return new (mem) hp_ring();
}

void hp_ring_destroy(hp_ring_t* ring) {
    if (!ring) return;
    ring->~hp_ring();
#ifdef _WIN32
    _aligned_free(ring);
#else
    ::operator delete(ring, std::align_val_t{HP_CACHELINE});
#endif
}

bool hp_ring_push(hp_ring_t* ring, const hp_result_t* result) {
    uint32_t h = ring->head.load(std::memory_order_relaxed);
    uint32_t t = ring->tail.load(std::memory_order_acquire);

    if (h - t >= HP_RING_CAPACITY) {
        return false; /* full */
    }

    ring->slots[h & (HP_RING_CAPACITY - 1)] = *result;
    ring->head.store(h + 1, std::memory_order_release);
    return true;
}

bool hp_ring_pop(hp_ring_t* ring, hp_result_t* out) {
    uint32_t t = ring->tail.load(std::memory_order_relaxed);
    uint32_t h = ring->head.load(std::memory_order_acquire);

    if (t >= h) {
        return false; /* empty */
    }

    *out = ring->slots[t & (HP_RING_CAPACITY - 1)];
    ring->tail.store(t + 1, std::memory_order_release);
    return true;
}

int32_t hp_ring_size(const hp_ring_t* ring) {
    uint32_t h = ring->head.load(std::memory_order_acquire);
    uint32_t t = ring->tail.load(std::memory_order_acquire);
    return (int32_t)(h - t);
}

} /* extern "C" */
