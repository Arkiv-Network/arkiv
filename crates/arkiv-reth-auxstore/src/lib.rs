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
//! Host-specific by design: the spec says only "there is a committed query index",
//! and *how* it is realized is reth's concern. Ported from the `arkiv-db-engine`
//! reference.
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
//!   bitmap. Values ≤ 32 bytes go through [`range`], backed by a storage-slot
//!   [`btree`]; longer strings go through the [`cascade`].
//!
//! ## Consensus
//!
//! The index is part of consensus and commits to its own contents, separately from
//! the entities'. Both the [`address`] derivations and the [`Bitmap`] serialization
//! are therefore **consensus-critical**: a change to either changes the state root,
//! so they are locked with golden test vectors and must never drift silently.
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
pub use annotation::QueryCapabilities;
pub use bitmap::{Bitmap, BitmapError};
pub use error::AuxError;
pub use range::Bound;
pub use storage::IndexStorage;
pub use store::RethAuxStore;
