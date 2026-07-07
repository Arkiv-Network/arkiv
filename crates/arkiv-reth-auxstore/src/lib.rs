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
//!   bitmap. (Lands in a later module.)
//!
//! ## Consensus
//!
//! The index is part of consensus and commits to its own contents, separately from
//! the entities'. Both the [`address`] derivations and the [`Bitmap`] serialization
//! are therefore **consensus-critical**: a change to either changes the state root,
//! so they are locked with golden test vectors and must never drift silently.
//!
//! Here so far: [`bitmap`] — the roaring64 entity-id set, and [`address`] — the
//! keccak-derived index bucket addresses. The write path (`apply_delta`) and read
//! path (`evaluate`) that use them land behind these primitives next.
//!
//! [`Bitmap`]: bitmap::Bitmap
//! [`pair_address`]: address::pair_address

pub mod address;
pub mod bitmap;

pub use address::{all_entities_bucket, pair_address};
pub use bitmap::{Bitmap, BitmapError};
