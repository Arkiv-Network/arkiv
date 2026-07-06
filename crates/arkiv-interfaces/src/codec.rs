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
//! The encoding is version-tagged (byte 0 is the format version), mirroring
//! `arkiv-da`'s `DA_VERSION`. There is **one** in-memory [`Entity`] — always the
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
    /// well-formed in-memory entity always encodes (mirrors `arkiv-da`'s
    /// `encode_bytes`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const V1: u8 = 1;
    const V2: u8 = 2;

    /// Error for the toy codec below — proves `type Error` only needs `Debug`.
    #[derive(Debug, PartialEq, Eq)]
    enum ToyErr {
        /// No version byte at all.
        Empty,
        /// Version tag the codec does not know.
        Unsupported(u8),
        /// The body ended mid-field or has trailing bytes.
        Malformed,
    }

    /// A dependency-free codec proving the trait is implementable and exercising
    /// the version story. It encodes only the fixed-size lifecycle fields (enough
    /// to demonstrate version dispatch + up-migration without a length-prefixed
    /// body): `v2` carries `last_modified_at_block`, the field the fictional `v1`
    /// lacked. Decoding a `v1` buffer migrates up by defaulting that field.
    struct ToyCodec;

    impl EntityCodec for ToyCodec {
        type Error = ToyErr;

        fn current_version(&self) -> u8 {
            V2
        }

        fn encode(&self, e: &Entity) -> Vec<u8> {
            let mut out = Vec::new();
            out.push(V2);
            out.extend_from_slice(&e.key);
            out.extend_from_slice(&e.creator);
            out.extend_from_slice(&e.owner);
            out.extend_from_slice(&e.created_at_block.to_be_bytes());
            out.extend_from_slice(&e.last_modified_at_block.to_be_bytes());
            out.extend_from_slice(&e.expires_at.to_be_bytes());
            out
        }

        fn decode(&self, bytes: &[u8]) -> Result<Entity, ToyErr> {
            let (&version, body) = bytes.split_first().ok_or(ToyErr::Empty)?;
            if version != V1 && version != V2 {
                return Err(ToyErr::Unsupported(version));
            }
            let mut r = Cursor::new(body);
            let key = r.arr::<32>()?;
            let creator = r.arr::<20>()?;
            let owner = r.arr::<20>()?;
            let created_at_block = u64::from_be_bytes(r.arr::<8>()?);
            // v1 predates `last_modified_at_block`; migrate it up with a default.
            let last_modified_at_block = if version == V2 {
                u64::from_be_bytes(r.arr::<8>()?)
            } else {
                0
            };
            let expires_at = u64::from_be_bytes(r.arr::<8>()?);
            if !r.done() {
                return Err(ToyErr::Malformed);
            }
            Ok(Entity {
                key,
                creator,
                owner,
                created_at_block,
                last_modified_at_block,
                expires_at,
                ..Entity::default()
            })
        }
    }

    /// Bounds-checked fixed-width reader over the codec body.
    struct Cursor<'a> {
        b: &'a [u8],
        p: usize,
    }

    impl<'a> Cursor<'a> {
        fn new(b: &'a [u8]) -> Self {
            Self { b, p: 0 }
        }
        fn arr<const N: usize>(&mut self) -> Result<[u8; N], ToyErr> {
            let end = self
                .p
                .checked_add(N)
                .filter(|e| *e <= self.b.len())
                .ok_or(ToyErr::Malformed)?;
            let mut a = [0u8; N];
            a.copy_from_slice(&self.b[self.p..end]);
            self.p = end;
            Ok(a)
        }
        fn done(&self) -> bool {
            self.p == self.b.len()
        }
    }

    /// An entity with empty content_type/payload/attributes, so the toy codec's
    /// lifecycle-only layout round-trips.
    fn sample() -> Entity {
        Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 10,
            last_modified_at_block: 20,
            expires_at: 100,
            ..Entity::default()
        }
    }

    #[test]
    fn encodes_current_version() {
        assert_eq!(ToyCodec.current_version(), V2);
        assert_eq!(ToyCodec.encode(&sample())[0], V2);
    }

    #[test]
    fn round_trips_current() {
        let e = sample();
        assert_eq!(ToyCodec.decode(&ToyCodec.encode(&e)).unwrap(), e);
    }

    #[test]
    fn decodes_and_migrates_v1() {
        // Hand-built v1 buffer: [1, key, creator, owner, created(8), expires(8)] —
        // note: no last_modified field.
        let mut b = vec![V1];
        b.extend_from_slice(&[7u8; 32]);
        b.extend_from_slice(&[1u8; 20]);
        b.extend_from_slice(&[2u8; 20]);
        b.extend_from_slice(&10u64.to_be_bytes());
        b.extend_from_slice(&100u64.to_be_bytes());

        let e = ToyCodec.decode(&b).unwrap();
        assert_eq!(e.last_modified_at_block, 0); // migrated up with a default
        assert_eq!(e.key, [7u8; 32]);
        assert_eq!(e.created_at_block, 10);
        assert_eq!(e.expires_at, 100);
    }

    #[test]
    fn rejects_unknown_version() {
        assert_eq!(ToyCodec.decode(&[0xFF]), Err(ToyErr::Unsupported(0xFF)));
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(ToyCodec.decode(&[]), Err(ToyErr::Empty));
    }

    #[test]
    fn decode_meta_projects_header() {
        let e = sample();
        let m = ToyCodec.decode_meta(&ToyCodec.encode(&e)).unwrap();
        assert_eq!(m.key, e.key);
        assert_eq!(m.owner, e.owner);
        assert_eq!(m.expires_at, e.expires_at);
    }
}
