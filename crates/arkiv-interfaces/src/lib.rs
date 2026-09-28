//! Traits for an **Arkiv-compatible state model**. no-std compatible

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod constants;
pub mod entity;
pub mod execution;
pub mod gas;
pub mod primitives;
pub mod query;
pub mod rpc;
pub mod statemanager;
pub mod store;

pub use constants::*;
pub use entity::*;
pub use execution::*;
pub use gas::*;
pub use primitives::*;
pub use query::*;
pub use rpc::*;
pub use statemanager::*;

// `store` is deliberately NOT glob re-exported: it names `Query`/`QueryResult`
// of its own, which are the store's wire shapes and not the Arkiv query AST in
// `query`. Reach them as `arkiv_interfaces::store::*`.
