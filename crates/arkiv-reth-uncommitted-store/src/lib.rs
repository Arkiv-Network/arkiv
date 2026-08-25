//! Arkiv's uncommitted, node-local stores: no commitment, no root, and always
//! rederivable from the committed stores — which is why in-memory is the
//! correct persistence, not a shortcut.

pub mod pruning_store;

pub use pruning_store::MemPruningStore;
