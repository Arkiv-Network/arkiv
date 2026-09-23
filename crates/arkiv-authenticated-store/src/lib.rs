//! Ordered authenticated storage for Arkiv-specific state.
//!
//! Balances and transaction nonces are deliberately absent. The host commits
//! [`State::root`] in Ethereum storage after persisting the immutable records.
//! See the crate README for the format, lifecycle, and integration boundaries.

pub mod state;
pub mod storage;
pub mod tree;

pub use state::State;
pub use storage::{MdbxStore, MemoryStore, RecordStore, Store};
pub use tree::{EMPTY_ROOT, ScanStats, Snapshot, Tree};
