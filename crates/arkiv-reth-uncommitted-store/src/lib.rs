//! The reth-host implementation of Arkiv's **uncommitted state** — the
//! node-local stores.
//!
//! Sibling to `arkiv-reth-mpt-committed-store`, covering the model's other
//! flavor: state that carries **no commitment** and feeds no root. Everything
//! here must be *rederivable from the committed stores*, so a restarted node
//! rebuilds it rather than trusting a sidecar — which is also why in-memory is
//! the correct persistence, not a shortcut.
//!
//! One module per store, at parity with the committed crate:
//!
//! | lane        | store             | module          |
//! |-------------|-------------------|-----------------|
//! | pruning map | [`MemPruningMap`] | [`pruning_map`] |
//!
//! This crate is deliberately a **placeholder home**: as more node-local
//! bookkeeping earns a store (caches over the committed lanes, purge work
//! queues, …), each lands here as its own module, never inlined into the state
//! manager — the manager composes stores, it doesn't own lane logic.

pub mod pruning_map;

pub use pruning_map::MemPruningMap;
