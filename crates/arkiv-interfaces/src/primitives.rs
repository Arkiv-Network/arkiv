//! The byte-array types every trait speaks in.
//!
//! Plain arrays, no primitive library: a host maps them to its own types at the
//! boundary, e.g. `let addr: Address = alloy_addr.into_array();` (and back with
//! `Address::from`).

/// A 20-byte address — an entity's owner or creator, or a transaction's caller.
pub type Address = [u8; 20];

/// A 32-byte hash — a commitment root, or the shape of an entity key.
pub type Hash = [u8; 32];

/// An entity's unique, stable identifier.
pub type EntityKey = Hash;

/// A block number. Expiry and lifecycle are counted in blocks, never wall-clock.
pub type BlockNumber = u64;

/// Gas / cost units. See [`CostModel`](crate::gas::CostModel).
pub type Gas = u64;
