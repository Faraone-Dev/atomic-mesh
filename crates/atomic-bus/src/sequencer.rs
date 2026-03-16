use atomic_core::event::Event;
use atomic_core::clock::SequenceGenerator;
use std::collections::VecDeque;

/// Event sequencer: assigns monotonically increasing sequence numbers
/// and maintains an ordered buffer of events.
pub struct EventSequencer {
    seq_gen: SequenceGenerator,
    buffer: VecDeque<Event>,
    max_buffer_size: usize,
}

impl EventSequencer {
    pub fn new(max_buffer_size: usize) -> Self {
        Self {
            seq_gen: SequenceGenerator::new(),
            buffer: VecDeque::with_capacity(max_buffer_size),
            max_buffer_size,
        }
    }

    /// Sequence an event: assign global seq number and buffer it.
    pub fn sequence(&mut self, mut event: Event) -> u64 {
        let seq = self.seq_gen.next();
        event.seq = seq;
        if self.buffer.len() >= self.max_buffer_size {
            self.buffer.pop_front();
        }
        self.buffer.push_back(event);
        seq
    }

    /// Drain all buffered events in order.
    pub fn drain(&mut self) -> Vec<Event> {
        self.buffer.drain(..).collect()
    }

    /// Peek at the next event without removing it.
    pub fn peek(&self) -> Option<&Event> {
        self.buffer.front()
    }

    /// How many events are buffered.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Current sequence counter value.
    pub fn current_seq(&self) -> u64 {
        self.seq_gen.peek() - 1
    }
}
