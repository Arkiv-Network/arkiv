//! The reth-host implementation of the Arkiv **auxiliary store** — the query
//! index over the entities.
//!
//! Sibling to `arkiv-reth-entitystore`. Where that crate implements
//! [`EntityStore`](arkiv_interfaces::state::EntityStore) — the entities
//! themselves — this crate implements
//! [`AuxiliaryStore`](arkiv_interfaces::state::AuxiliaryStore): give it a
//! [`Query`](arkiv_interfaces::query::Query) and it returns the keys of the
//! entities that match.
//!
//! ## Host-specific by design
//!
//! Per the Arkiv/host boundary (see the `arkiv-interfaces` crate docs), the spec
//! only says "there is a committed query index that answers the query language".
//! *How* that index is physically realized — bitmaps of entity ids stored at
//! keccak-derived account addresses, ordered values in storage-slot B+ trees — is
//! reth's concern. This crate is one concrete answer to that on reth, ported from
//! the proven `arkiv-db-engine` reference.
//!
//! ## The index, in two tiers
//!
//! - **Tier 1 — equality.** Every `(attribute, value)` pair maps to a *pair
//!   account* at [`pair_address`](address::pair_address), whose contents are a
//!   [`Bitmap`](bitmap::Bitmap) of the entity ids carrying that pair. Equality
//!   (`Eq`/`In`) and their negations are answered by reading and combining these
//!   bitmaps. This is the whole index for keys whose values aren't range-queried.
//! - **Tier 2 — range.** For range-queried keys (`$expiration`, `$createdAtBlock`,
//!   uint/string attributes) an ordered structure over the *values* lets `Gt`/`Lt`
//!   scans enumerate the matching values, each of which resolves back to its tier-1
//!   bitmap. The int-mode half — values ≤ 32 bytes, backed by a storage-slot
//!   [`btree`] — is [`range`]; the string cascade for longer values lands next.
//!
//! ## Consensus
//!
//! The index is part of consensus and commits to its own contents, separately from
//! the entities'. Both the [`address`] derivations and the [`Bitmap`] serialization
//! are therefore **consensus-critical**: a change to either changes the state root,
//! so they are locked with golden test vectors and must never drift silently.
//!
//! The primitives: [`bitmap`] — the roaring64 entity-id set; [`address`] — the
//! keccak-derived index bucket addresses; [`storage`] — the [`IndexStorage`] seam
//! the tier-2 index writes through; [`btree`] — the int-mode B+ tree over that seam;
//! [`range`] — the caller-facing int range index (encode a value, scan a bound); and
//! [`cascade`] — the str-mode counterpart, a chunk-by-chunk cascade for string
//! values up to 128 bytes.
//!
//! And the store that combines them into an [`AuxiliaryStore`]: [`annotation`] —
//! which physical index a `(attr, value)` pair uses, shared by both paths;
//! [`index`] — the write path folding a delta into the bitmaps and tier-2
//! structures; [`interpret`] — the read path walking a [`Query`] to a bitmap of ids;
//! and [`store`] — [`RethAuxStore`], the [`AuxiliaryStore`] impl that owns the
//! id→key map and pages query results.
//!
//! [`Bitmap`]: bitmap::Bitmap
//! [`pair_address`]: address::pair_address
//! [`IndexStorage`]: storage::IndexStorage
//! [`AuxiliaryStore`]: arkiv_interfaces::state::AuxiliaryStore
//! [`Query`]: arkiv_interfaces::query::Query

pub mod address;
pub mod annotation;
pub mod bitmap;
pub mod btree;
pub mod cascade;
pub mod range;
pub mod storage;
pub mod store;

mod error;
mod index;
mod interpret;
mod slot;

pub use address::{all_entities_bucket, pair_address};
pub use annotation::Mode;
pub use bitmap::{Bitmap, BitmapError};
pub use error::AuxError;
pub use range::Bound;
pub use storage::IndexStorage;
pub use store::RethAuxStore;
