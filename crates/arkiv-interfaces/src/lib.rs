//! Traits for an **Arkiv-compatible state model**.
//!
//! Arkiv is an on-chain database of *entities*: owned, expiring records with
//! queryable attributes. It runs today inside a custom reth build, but the model
//! itself shouldn't care who hosts it. This crate is that boundary — the traits a
//! host implements, plus the plain data types they exchange. It names no reth,
//! revm, alloy, or storage backend.
//!
//! ## No dependencies
//!
//! The crate is `#![no_std]` (only [`alloc`], for `Vec`/`String`/`Box`) and pulls
//! in **no external crates**. Two consequences:
//!
//! - Values are fixed-width byte arrays ([`primitives`]), not some library's
//!   types. A host converts at the boundary — `[u8; 20]` ⇄ an alloy `Address` is
//!   free.
//! - Errors are an associated `type Error` on every trait, bounded only by
//!   [`core::fmt::Debug`], so a host can use `eyre`, `anyhow`, its own enum, …
//!
//! Encoding (RLP, the index's on-disk form, …) is a host detail and lives in the
//! host's crate, never here.
//!
//! ## The pieces
//!
//! Two **stores** hold all state. Both are part of consensus, and each commits to
//! its own contents separately:
//!
//! - [`EntityStore`] — the entities, stored as bytes.
//! - [`AuxiliaryStore`] — the query index over them.
//!
//! [`EntityCodec`] is the version-tagged contract for turning an entity into those
//! stored bytes and back — a consensus format, so it lives here even though its
//! implementation (RLP, …) does not.
//!
//! A host that serves history also implements the optional
//! [`HistoricalEntityStore`] and [`HistoricalAuxiliaryStore`], which add
//! past-block reads.
//!
//! Three **operations** run against those stores:
//!
//! - [`TransactionExecutor`] — runs one transaction, staging its changes into a
//!   [`BlockDraft`].
//! - [`BlockExecutor`] — runs a whole block, applies the draft, and returns the
//!   block's commitments.
//! - [`QueryProcessor`] — answers a query at the tip with a page of entities and
//!   the statistics of the work it did. A host may also implement
//!   [`HistoricalQuery`] to answer as of a past block.
//!
//! [`CostModel`] is where gas pricing will live; for now it's a seam with a
//! zero-cost [`PlaceholderCost`].
//!
//! Finally, [`ArkivRpc`] is the `arkiv_*` surface a node serves to clients (with
//! [`ArkivHistoricalRpc`] for nodes that answer against past blocks).
//!
//! ## Constants
//!
//! [`constants`] holds the numbers more than one crate has to agree on — the byte
//! widths the encodings are built on, the protocol limits the ABI encoder and the
//! engine both enforce, and [`ARKIV_ADDRESS`]. A value
//! only one crate reads stays in that crate.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod codec;
pub mod constants;
pub mod entity;
pub mod execution;
pub mod gas;
pub mod primitives;
pub mod query;
pub mod rpc;
pub mod state;

pub use codec::*;
pub use constants::*;
pub use entity::*;
pub use execution::*;
pub use gas::*;
pub use primitives::*;
pub use query::*;
pub use rpc::*;
pub use state::*;

#[cfg(test)]
mod tests;
