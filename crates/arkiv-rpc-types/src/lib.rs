//! The `arkiv_*` JSON-RPC **wire contract**.
//!
//! Four methods make up the read surface an SDK depends on:
//!
//! | method | request | response |
//! |---|---|---|
//! | `arkiv_getEntity` | `[key]` or `[key, block]` | [`EntityData`] or `null` |
//! | `arkiv_query` | `[q]` or `[q, `[`QueryOptions`]`]` | [`QueryResponse`] |
//! | `arkiv_getEntityCount` | `[`[`CountRequest`]`]` (optional) | a hex quantity |
//! | `arkiv_getBlockTiming` | `[]` | [`BlockTimingView`] |
//!
//! ## Why this is its own crate
//!
//! These shapes are a *contract*, and a contract with one implementation and
//! several readers belongs where both sides can reach it. The node serves them
//! (`arkiv-reth-rpc`); `arkiv-cli`, `arkiv-harness` and any Rust SDK read them.
//! Kept inside the node they would be unreachable, and every client would
//! re-derive the field names and encodings from prose — which is exactly how a
//! client and a server drift.
//!
//! So this crate names **no transport**: no jsonrpsee, no reth, no HTTP. It is
//! `serde` shapes plus the rules for filling them in. What a node decides *at
//! request time* — which snapshot to read, whether a cursor is resumable — is the
//! server's business and lives in `arkiv-reth-rpc`.
//!
//! ## The pieces
//!
//! - [`entity`] — how a stored entity is projected onto the wire. Reads are
//!   **opt-in**: a caller names the fields it wants ([`Select`]), and everything
//!   else is left off the response rather than sent as null.
//! - [`method`] — the per-method request and response shapes, and the page-size
//!   bounds `arkiv_query` enforces.
//! - [`error`] — the frozen error-code table an SDK branches on.

#![forbid(unsafe_code)]

pub mod entity;
pub mod error;
pub mod method;

pub use entity::*;
pub use error::*;
pub use method::*;
