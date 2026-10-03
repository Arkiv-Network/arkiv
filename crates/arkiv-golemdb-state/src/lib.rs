//! Arkiv's committed stores over a GolemDB `Store`, with entities as native records.
//!
//! Replaces `arkiv-reth-mpt-committed-store`. Most of that crate is index machinery
//! built because Ethereum's account trie has none; a GolemDB store indexes attribute
//! cells itself, so [`indices`] is a lowering into store queries and nothing more.
#![no_std]

extern crate alloc;

pub mod accounts;
pub mod entities;
pub mod indices;
pub mod manager;
pub mod view;

pub use manager::GolemStateManager;
pub use view::{GolemStateView, ViewError};

/// The node carries one erased store handle, so nothing downstream has to be
/// generic over the store type: `StoreExt` is object-safe, a trait object
/// implements its supertrait `Store`, and `Arc<T: Store>` is itself a `Store`.
#[allow(dead_code)]
fn _erased_handle_is_a_store(store: alloc::sync::Arc<dyn arkiv_interfaces::store::StoreExt>) {
    let _: &dyn arkiv_interfaces::store::Store = &store;
}
