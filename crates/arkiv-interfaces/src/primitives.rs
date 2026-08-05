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

/// An owner's **entity-minting** nonce — how many entities that account has
/// created, and an input to every minted [`EntityKey`].
///
/// A distinct type because there are two unrelated nonces in play and mixing
/// them silently mints the wrong key: this one, and the account's ordinary
/// **transaction** nonce (reth's, incremented per transaction). They advance at
/// different rates — one batch can create three entities while bumping the tx
/// nonce once — so they are never interchangeable, and a bare `u64` on both
/// gives the compiler no way to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityNonce(u64);

impl EntityNonce {
    /// The nonce of an account that has never created an entity.
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The nonce `count` creates later. Saturating: an account cannot realistically
    /// reach `u64::MAX` entities, and wrapping there would remint live keys.
    pub const fn advanced_by(self, count: u64) -> Self {
        Self(self.0.saturating_add(count))
    }
}
