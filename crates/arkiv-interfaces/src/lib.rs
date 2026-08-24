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

pub use constants::*;
pub use entity::*;
pub use execution::*;
pub use gas::*;
pub use primitives::*;
pub use query::*;
pub use rpc::*;
pub use statemanager::*;
