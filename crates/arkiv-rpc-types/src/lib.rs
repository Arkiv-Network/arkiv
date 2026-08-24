//! The `arkiv_*` JSON-RPC wire contract.
//!
//! | method | request | response |
//! |---|---|---|
//! | `arkiv_getEntity` | `[key]` or `[key, block]` | [`EntityData`] or `null` |
//! | `arkiv_query` | `[q]` or `[q, `[`QueryOptions`]`]` | [`QueryResponse`] |
//! | `arkiv_getEntityCount` | `[`[`CountRequest`]`]` (optional) | a hex quantity |
//! | `arkiv_getBlockTiming` | `[]` | [`BlockTimingView`] |
//!
//! ## Errors
//!
//! | code | `data` |
//! |---|---|
//! | [`MALFORMED_INPUT`] | `position`, `message` |
//! | [`TYPE_ERROR`] | `position`, `message` |
//! | [`LITERAL_ERROR`] | `position`, `message` |
//! | [`QUERY_LIMIT`] | `message` |
//! | [`CURSOR_ERROR`] | `message` |
//! | [`BLOCK_UNAVAILABLE`] | `requested`, `latest`, `message` |
//!
//! `position` is a byte offset into the query text.

#![forbid(unsafe_code)]

pub mod entity;
pub mod method;

pub use entity::*;
pub use method::*;
