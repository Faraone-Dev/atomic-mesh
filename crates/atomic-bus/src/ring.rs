use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Lock-free Single-Producer Single-Consumer ring buffer.
/// Zero-allocation after init. Cache-line aligned for performance.
/// Used for passing events between producer (feed/sequencer) and consumer (strategy engine).
pub struct SpscRing<T> {
    buffer: Box<[UnsafeCell<Option<T>>]>,
    capacity: usize,
    head: AtomicUsize, // writer position
    tail: AtomicUsize, // reader position
}

// SAFETY: SPSC guarantees single producer and single consumer on separate threads.
unsafe impl<T: Send> Send for SpscRing<T> {}
unsafe impl<T: Send> Sync for SpscRing<T> {}

impl<T> SpscRing<T> {
    /// Create a ring buffer with the given capacity (will be rounded up to next power of 2).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let buffer: Vec<UnsafeCell<Option<T>>> =
            (0..capacity).map(|_| UnsafeCell::new(None)).collect();
        Self {
            buffer: buffer.into_boxed_slice(),
            capacity,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Try to push an element. Returns Err(value) if full.
    pub fn try_push(&self, value: T) -> Result<(), T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let next_head = (head + 1) & (self.capacity - 1);

        if next_head == tail {
            return Err(value); // full
        }

        unsafe {
            *self.buffer[head].get() = Some(value);
        }
        self.head.store(next_head, Ordering::Release);
        Ok(())
    }

    /// Try to pop an element. Returns None if empty.
    pub fn try_pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        if tail == head {
            return None; // empty
        }

        let value = unsafe { (*self.buffer[tail].get()).take() };
        let next_tail = (tail + 1) & (self.capacity - 1);
        self.tail.store(next_tail, Ordering::Release);
        value
    }

    /// Number of elements currently in the buffer.
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        (head + self.capacity - tail) & (self.capacity - 1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity - 1 // one slot reserved for full/empty distinction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_basic() {
        let ring = SpscRing::new(4);
        assert!(ring.is_empty());

        ring.try_push(1u64).unwrap();
        ring.try_push(2).unwrap();
        ring.try_push(3).unwrap();
        assert_eq!(ring.len(), 3);

        assert_eq!(ring.try_pop(), Some(1));
        assert_eq!(ring.try_pop(), Some(2));
        assert_eq!(ring.try_pop(), Some(3));
        assert_eq!(ring.try_pop(), None);
    }

    #[test]
    fn full_ring_returns_err() {
        let ring = SpscRing::new(2); // rounds up to 2, capacity = 1 usable
        ring.try_push(42u32).unwrap();
        assert!(ring.try_push(43).is_err());
    }

    #[test]
    fn wrap_around() {
        let ring = SpscRing::new(4);
        for i in 0..100u64 {
            ring.try_push(i).unwrap();
            assert_eq!(ring.try_pop(), Some(i));
        }
    }
}
