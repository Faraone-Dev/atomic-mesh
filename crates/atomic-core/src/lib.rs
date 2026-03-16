// ATOMIC MESH — Core Types & Deterministic Event Engine
//
// Every event in the system flows through this module.
// Rules:
//   1. Monotonic global sequence — no gaps, no reordering
//   2. Single-threaded deterministic processing per node
//   3. No unseeded randomness
//   4. No live clock access in hot path — only logical time
//   5. Same events → same state, always

pub mod event;
pub mod types;
pub mod clock;
pub mod node;
pub mod error;
pub mod snapshot;
pub mod metrics;

pub use event::*;
pub use types::*;
pub use clock::*;
pub use node::*;
pub use error::*;
pub use snapshot::*;
