//! The `arkiv_*` JSON-RPC wire contract.
//!
//! | method | request | response |
//! |---|---|---|
//! | `arkiv_getEntity` | `[key]` or `[key, block]` | [`EntityData`] or `null` |
//! | `arkiv_query` | `[q]` or `[q, `[`QueryOptions`]`]` | [`QueryResponse`] |
//! | `arkiv_getEntityCount` | `[`[`CountRequest`]`]` (optional) | a hex quantity |
//! | `arkiv_getBlockTiming` | `[]` | [`BlockTimingView`] |
//!
//! Its own crate because both sides need it: the node serves these shapes
//! (`arkiv-reth-rpc`), and `arkiv-cli`, `arkiv-harness` and any Rust SDK read
//! them. So it names no transport — no jsonrpsee, no reth, no HTTP.

#![forbid(unsafe_code)]

pub mod entity;
pub mod error;
pub mod method;

pub use entity::*;
pub use error::*;
pub use method::*;
