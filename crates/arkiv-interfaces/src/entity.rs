//! The entity data types passed across the traits.
//!
//! Plain data, no encoding: turning an [`Entity`] into the bytes an
//! [`EntityStore`](crate::state::EntityStore) holds (and back) is the host's job.

use alloc::vec::Vec;

use crate::primitives::{Address, BlockNumber, EntityKey};

/// A stored entity: a fixed identity, a changing lifecycle, opaque contents, and
/// queryable attributes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entity {
    /// Unique key. Set at creation, never changes.
    pub key: EntityKey,
    /// Who created it. Set at creation, never changes.
    pub creator: Address,
    /// Who owns it now. Changes on transfer.
    pub owner: Address,
    /// The block it was created in.
    pub created_at_block: BlockNumber,
    /// The block of its last change (create, update, extend, or transfer).
    pub last_modified_at_block: BlockNumber,
    /// The block it expires in.
    pub expires_at: BlockNumber,
    /// Opaque content type (e.g. a MIME string).
    pub content_type: Vec<u8>,
    /// Opaque application payload.
    pub payload: Vec<u8>,
    /// Queryable attributes.
    pub attributes: Vec<Attribute>,
}

/// An [`Entity`] without its payload or attributes — enough for a query that only
/// needs identity and lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntityMeta {
    pub key: EntityKey,
    pub creator: Address,
    pub owner: Address,
    pub created_at_block: BlockNumber,
    pub last_modified_at_block: BlockNumber,
    pub expires_at: BlockNumber,
    pub content_type: Vec<u8>,
}

/// A `(key, value)` attribute. `value_type` says how to read `value` — see the
/// `ATTR_*` tags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attribute {
    pub key: Vec<u8>,
    pub value_type: u8,
    pub value: Vec<u8>,
}

/// `value_type`: a 32-byte big-endian integer.
pub const ATTR_UINT: u8 = 1;
/// `value_type`: opaque bytes (a UTF-8 string, by convention).
pub const ATTR_STRING: u8 = 2;
/// `value_type`: a 32-byte entity key.
pub const ATTR_ENTITY_KEY: u8 = 3;

/// The attribute names the protocol sets on every entity.
///
/// Some never change (`$creator`, `$createdAtBlock`, `$key`); `$owner` changes on
/// transfer, `$expiration` on extend, `$contentType` on update. `$all` matches
/// every live entity.
pub mod annotations {
    /// Matches every live entity.
    pub const ALL: &[u8] = b"$all";
    /// The creator's address (never changes).
    pub const CREATOR: &[u8] = b"$creator";
    /// The creation block, as a big-endian `u64` (never changes).
    pub const CREATED_AT_BLOCK: &[u8] = b"$createdAtBlock";
    /// The current owner (changes on transfer).
    pub const OWNER: &[u8] = b"$owner";
    /// The full entity key, 32 raw bytes (never changes).
    pub const KEY: &[u8] = b"$key";
    /// The expiry block, as a big-endian `u64` (changes on extend).
    pub const EXPIRATION: &[u8] = b"$expiration";
    /// The content type (changes on update).
    pub const CONTENT_TYPE: &[u8] = b"$contentType";
}
