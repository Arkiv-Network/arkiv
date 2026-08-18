//! Turning an [`Entity`] into the version-tagged bytes a store persists.
//!
//! An [`EntityStore`](crate::state::EntityStore) holds opaque bytes and commits to
//! a hash over them; the encoding used is therefore a **consensus** artifact —
//! every node must serialize identically. [`EntityCodec`] is that standardized
//! contract, kept separate from the store (and the executor) so a single,
//! version-tagged format is shared by everyone who reads or writes entity bytes.
//!
//! ## Versioning
//!
//! The encoding is version-tagged (byte 0 is the format version). There is
//! **one** in-memory [`Entity`] — always the
//! latest schema. [`EntityCodec::decode`] reads the tag, parses that version's
//! layout, and migrates the result *up* to the canonical [`Entity`], defaulting
//! any field newer than the stored version. [`EntityCodec::encode`] always writes
//! the current version. So `EntityV1`, `EntityV2`, … are never separate in-memory
//! types the executor or queries must know about — they are just past on-disk
//! layouts the codec knows how to read. Freeze each version once shipped.

use alloc::vec::Vec;

use crate::entity::{Entity, EntityMeta};

/// Serializes entities to and from the bytes a store persists. See the
/// [module docs](self) for the versioning model.
pub trait EntityCodec {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The version [`encode`](EntityCodec::encode) currently writes (byte 0 of its
    /// output).
    fn current_version(&self) -> u8;

    /// Serialize `entity` to stored bytes in the current version. Infallible: a
    /// well-formed in-memory entity always encodes.
    fn encode(&self, entity: &Entity) -> Vec<u8>;

    /// Parse stored bytes of any supported version into the canonical [`Entity`],
    /// migrating older layouts up. `Err` on an unknown version tag or malformed
    /// body.
    fn decode(&self, bytes: &[u8]) -> Result<Entity, Self::Error>;

    /// Cheap header-only decode for query paths that need identity and lifecycle
    /// but not the payload or attributes. Defaults to a full [`decode`](
    /// EntityCodec::decode) projected onto [`EntityMeta`]; an implementation may
    /// override it with a faster path that skips the body.
    fn decode_meta(&self, bytes: &[u8]) -> Result<EntityMeta, Self::Error> {
        let e = self.decode(bytes)?;
        Ok(EntityMeta {
            key: e.key,
            creator: e.creator,
            owner: e.owner,
            created_at_block: e.created_at_block,
            last_modified_at_block: e.last_modified_at_block,
            expires_at: e.expires_at,
            content_type: e.content_type,
        })
    }
}
