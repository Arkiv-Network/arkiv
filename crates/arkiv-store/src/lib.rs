//! Arkiv's database state under its own root.
//!
//! Three persistent tries share one content-addressed node store:
//!
//! | trie      | key                                   | value            |
//! |-----------|---------------------------------------|------------------|
//! | entities  | entity key (32 bytes, unhashed)       | entity record    |
//! | nonces    | owner address (20 bytes)              | next nonce, u64  |
//! | indexes   | attribute name `++ 0x00 ++` type id   | that index's root|
//!
//! and each index is itself a trie keyed by `order-preserving value ++ entity
//! key`, so equality is a prefix walk, `STARTSWITH` a shorter prefix walk, and
//! a range an ordered walk. The three roots hash into one top node, and that
//! hash is the **database root** the Ethereum anchor account's slot 0 holds.
//!
//! Reads go through a [`DbView`] at a root; a batch of changes becomes a new
//! root through [`DbView::commit`]. Nothing is ever mutated in place, so any
//! root the node store still holds stays readable.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod annotations;
pub mod encoding;
pub mod query;
pub mod record;
pub mod roots;
pub mod turso;
pub mod view;

pub use alloy_primitives::{Address, B256, U256};
pub use annotations::{AttrEntry, EntityDelta, annotation_delta, entity_annotations};
pub use arkiv_trie::{
    CountingReader, MemNodeStore, NodeReader, NodeSink, NodeStore, SharedMemNodeStore, Staging,
};
pub use query::{evaluate, evaluate_page};
pub use record::{RecordError, decode, encode};
pub use roots::{DbRoots, EMPTY_DB_ROOT};
pub use turso::{ArkivDb, DbError};
pub use view::{DbChanges, DbView, StoreError, commit_changes};

/// The Ethereum account whose storage slot 0 holds the database root. The
/// address is the ASCII string `arkiv-database-root!`, exactly 20 bytes, so it
/// is recognizable in an explorer and no key can collide with it.
pub const ARKIV_ROOT_ACCOUNT: Address = Address::new(*b"arkiv-database-root!");

/// The slot of [`ARKIV_ROOT_ACCOUNT`] holding the database root.
pub const ARKIV_ROOT_SLOT: U256 = U256::ZERO;
