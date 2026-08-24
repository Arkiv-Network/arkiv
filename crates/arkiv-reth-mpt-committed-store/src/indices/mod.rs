//! The query index over the entities.
//!
//! Where [`entities`](crate::entities) holds the entities themselves, this
//! module answers queries about them: give it a
//! [`Query`](arkiv_interfaces::query::Query) — or one of the spec's index
//! primitives (equality, prefix, range) — and it returns the keys of the
//! entities that match.
//!
//! Host-specific by design: the spec says only "there is a committed query index",
//! and *how* it is realized is reth's concern. Ported from the `arkiv-db-engine`
//! reference.
//!
//! ## Two indexes inside
//!
//! Internally the store is the composition of two sub-indexes (the private
//! `EqualityIndex` and `RangeIndex` types):
//!
//! - **The equality index.** Every `(attribute, value)` pair maps to a *pair
//!   account* at [`pair_address`](address::pair_address), whose contents are a
//!   [`Bitmap`](bitmap::Bitmap) of the entity ids carrying that pair. Equality
//!   (`Eq`/`In`) and their negations are answered by reading and combining these
//!   bitmaps. This is the whole index for keys whose values aren't range-queried.
//! - **The range index.** For range-queried keys (`$expiration`,
//!   `$createdAtBlock`, uint/string attributes) an ordered structure over the
//!   *values* lets `Gt`/`Lt` scans enumerate the matching values, each of which
//!   resolves back to its equality bitmap. Values ≤ 32 bytes go through
//!   [`range`], backed by a storage-slot [`btree`]; longer strings go through
//!   the [`cascade`]. It holds values only, never entity ids, and is touched
//!   only when a value's equality bitmap crosses the empty boundary.
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
//! [`Query`]: arkiv_interfaces::query::Query

pub mod address;
pub mod annotation;
pub mod bitmap;
pub mod btree;
pub mod cascade;
pub mod delta;
pub mod range;
pub mod storage;
pub mod store;

mod equality_index;
mod error;
mod index;
mod interpret;
mod range_index;
mod slot;

pub use address::{all_entities_bucket, pair_address};
pub use annotation::QueryCapabilities;
pub use bitmap::{Bitmap, BitmapError};
pub use delta::{AttrEntry, AuxiliaryEntityDelta};
pub use error::AuxError;
pub use range::Bound;
pub use storage::IndexStorage;
pub use store::RethAuxStore;
