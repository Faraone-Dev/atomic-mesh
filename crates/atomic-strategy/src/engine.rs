use atomic_core::event::Event;
use atomic_orderbook::OrderBook;
use crate::traits::{Strategy, StrategyCommand, StrategyContext};
use std::collections::HashMap;

/// Strategy engine: manages multiple strategies and dispatches events.
pub struct StrategyEngine {
    strategies: Vec<Box<dyn Strategy>>,
    books: HashMap<String, OrderBook>,
    positions: HashMap<String, atomic_core::types::Position>,
}

impl StrategyEngine {
    pub fn new() -> Self {
        Self {
            strategies: Vec::new(),
            books: HashMap::new(),
            positions: HashMap::new(),
        }
    }

    pub fn register(&mut self, strategy: Box<dyn Strategy>) {
        strategy.id(); // verify it has an id
        self.strategies.push(strategy);
    }

    pub fn update_book(&mut self, key: String, book: OrderBook) {
        self.books.insert(key, book);
    }

    pub fn update_position(&mut self, key: String, pos: atomic_core::types::Position) {
        self.positions.insert(key, pos);
    }

    /// Process an event through all registered strategies.
    /// Returns aggregated commands from all strategies.
    pub fn process_event(&mut self, event: &Event) -> Vec<StrategyCommand> {
        let ctx = StrategyContext {
            books: &self.books,
            positions: &self.positions,
            seq: event.seq,
            timestamp: event.timestamp,
        };

        let mut commands = Vec::new();
        for strategy in &mut self.strategies {
            let cmds = strategy.on_event(event, &ctx);
            commands.extend(cmds);
        }
        commands
    }

    pub fn start_all(&mut self) {
        for s in &mut self.strategies {
            s.on_start();
        }
    }

    pub fn stop_all(&mut self) {
        for s in &mut self.strategies {
            s.on_stop();
        }
    }

    pub fn strategy_count(&self) -> usize {
        self.strategies.len()
    }
}

impl Default for StrategyEngine {
    fn default() -> Self {
        Self::new()
    }
}
