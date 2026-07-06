//! Entity record codec — an [`Entity`] to and from the bytes an entity account's
//! `code` holds.
//!
//! Format: `0xFE || version || RLP(entity fields)`. `0xFE` is the EVM `INVALID`
//! opcode, so a `CALL` to an entity account halts immediately (entities are data,
//! never executed); the next byte is the **record version** — currently `0x00`, so
//! the prefix is `0xFE00` today. [`decode`] dispatches on the version and migrates
//! older layouts up to the canonical [`Entity`] — the same versioning model as the
//! spec's [`EntityCodec`]. The RLP **field order is consensus-critical**: it hashes
//! into the state root, so a shipped version must never change — add a new version
//! instead.
//!
//! This is host-specific (RLP + account code is reth's storage form), which is why
//! it lives in the entity-store crate, not the spec. [`RecordCodec`] implements the
//! spec's [`EntityCodec`] over these functions.

use core::fmt;

use alloy_primitives::{Address, B256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};

use arkiv_interfaces::codec::EntityCodec;
use arkiv_interfaces::entity::{Attribute, Entity};

/// Marker byte before an entity record — the EVM `INVALID` opcode, so a `CALL` to
/// an entity account halts.
pub const ENTITY_CODE_MARKER: u8 = 0xFE;

/// Current record format version, written right after the marker. Bump it when the
/// on-code layout changes; [`decode`] keeps reading older versions.
pub const RECORD_VERSION: u8 = 0x00;

/// Encode an entity to its stored-code bytes: `0xFE || RECORD_VERSION || RLP`.
pub fn encode(entity: &Entity) -> Vec<u8> {
    let rlp = EntityRlp::from_entity(entity);
    let mut out = Vec::with_capacity(2 + rlp.length());
    out.push(ENTITY_CODE_MARKER);
    out.push(RECORD_VERSION);
    rlp.encode(&mut out);
    out
}

/// Decode an entity from its stored-code bytes: verify the `0xFE` marker, read the
/// version byte, and decode that version's layout.
pub fn decode(code: &[u8]) -> Result<Entity, RecordError> {
    if code.first() != Some(&ENTITY_CODE_MARKER) {
        return Err(RecordError::MissingPrefix);
    }
    let Some(&version) = code.get(1) else {
        return Err(RecordError::MissingPrefix);
    };
    match version {
        RECORD_VERSION => decode_v0(&code[2..]),
        v => Err(RecordError::UnsupportedVersion(v)),
    }
}

/// Decode a version-`0` body. v0 is the canonical layout today, so this is a
/// straight RLP decode; a future version would parse its own layout here and
/// migrate the fields up to the canonical [`Entity`].
fn decode_v0(mut body: &[u8]) -> Result<Entity, RecordError> {
    let rlp = EntityRlp::decode(&mut body).map_err(RecordError::Rlp)?;
    if !body.is_empty() {
        return Err(RecordError::TrailingBytes);
    }
    Ok(rlp.into_entity())
}

/// Why decoding an entity record failed.
#[derive(Debug)]
pub enum RecordError {
    /// The code didn't start with the `0xFE` marker + version prefix.
    MissingPrefix,
    /// The version byte names a record layout this codec doesn't know.
    UnsupportedVersion(u8),
    /// The RLP body was malformed.
    Rlp(alloy_rlp::Error),
    /// Extra bytes followed the RLP body.
    TrailingBytes,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::MissingPrefix => {
                write!(f, "entity code is missing the 0xFE + version prefix")
            }
            RecordError::UnsupportedVersion(v) => {
                write!(f, "unsupported entity record version {v:#04x}")
            }
            RecordError::Rlp(e) => write!(f, "entity RLP decode failed: {e}"),
            RecordError::TrailingBytes => write!(f, "trailing bytes after the entity record"),
        }
    }
}

impl std::error::Error for RecordError {}

/// The concrete [`EntityCodec`] for reth-hosted entities — the version-tagged
/// `0xFE || version || RLP` format above. The version byte follows the `0xFE`
/// marker (so it's byte 1, not byte 0).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecordCodec;

impl EntityCodec for RecordCodec {
    type Error = RecordError;

    fn current_version(&self) -> u8 {
        RECORD_VERSION
    }

    fn encode(&self, entity: &Entity) -> Vec<u8> {
        encode(entity)
    }

    fn decode(&self, bytes: &[u8]) -> Result<Entity, RecordError> {
        decode(bytes)
    }
}

// ── On-code representation (RLP field order is consensus-critical) ──────

/// The entity as it is RLP-encoded into account code. Field order and types match
/// db-engine's `EntityRlp` exactly — do not reorder.
#[derive(RlpEncodable, RlpDecodable)]
struct EntityRlp {
    payload: Vec<u8>,
    creator: Address,
    created_at_block: u64,
    owner: Address,
    expires_at: u64,
    content_type: Vec<u8>,
    key: B256,
    attributes: Vec<AttributeRlp>,
    last_modified_at_block: u64,
}

#[derive(RlpEncodable, RlpDecodable)]
struct AttributeRlp {
    key: Vec<u8>,
    value_type: u8,
    value: Vec<u8>,
}

impl EntityRlp {
    fn from_entity(e: &Entity) -> Self {
        Self {
            payload: e.payload.clone(),
            creator: e.creator.into(),
            created_at_block: e.created_at_block,
            owner: e.owner.into(),
            expires_at: e.expires_at,
            content_type: e.content_type.clone(),
            key: e.key.into(),
            attributes: e.attributes.iter().map(AttributeRlp::from_attr).collect(),
            last_modified_at_block: e.last_modified_at_block,
        }
    }

    fn into_entity(self) -> Entity {
        Entity {
            key: self.key.into(),
            creator: self.creator.into(),
            owner: self.owner.into(),
            created_at_block: self.created_at_block,
            last_modified_at_block: self.last_modified_at_block,
            expires_at: self.expires_at,
            content_type: self.content_type,
            payload: self.payload,
            attributes: self
                .attributes
                .into_iter()
                .map(AttributeRlp::into_attr)
                .collect(),
        }
    }
}

impl AttributeRlp {
    fn from_attr(a: &Attribute) -> Self {
        Self {
            key: a.key.clone(),
            value_type: a.value_type,
            value: a.value.clone(),
        }
    }
    fn into_attr(self) -> Attribute {
        Attribute {
            key: self.key,
            value_type: self.value_type,
            value: self.value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT};

    fn sample() -> Entity {
        Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 10,
            last_modified_at_block: 20,
            expires_at: 100,
            content_type: b"text/plain".to_vec(),
            payload: b"hello world".to_vec(),
            attributes: vec![
                Attribute {
                    key: b"color".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"blue".to_vec(),
                },
                Attribute {
                    key: b"size".to_vec(),
                    value_type: ATTR_UINT,
                    value: vec![0u8; 32],
                },
                Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: vec![9u8; 32],
                },
            ],
        }
    }

    #[test]
    fn roundtrips_full_entity() {
        let e = sample();
        let bytes = encode(&e);
        assert_eq!(&bytes[..2], &[ENTITY_CODE_MARKER, RECORD_VERSION]); // 0xFE00
        assert_eq!(decode(&bytes).unwrap(), e);
    }

    #[test]
    fn roundtrips_minimal_entity() {
        let e = Entity {
            key: [3u8; 32],
            ..Entity::default()
        };
        assert_eq!(decode(&encode(&e)).unwrap(), e);
    }

    #[test]
    fn encoding_is_deterministic() {
        assert_eq!(encode(&sample()), encode(&sample()));
    }

    #[test]
    fn rejects_missing_prefix() {
        let mut bytes = encode(&sample());
        bytes[0] = 0x00;
        assert!(matches!(decode(&bytes), Err(RecordError::MissingPrefix)));
        assert!(matches!(decode(&[]), Err(RecordError::MissingPrefix)));
        // Marker present but no version byte.
        assert!(matches!(
            decode(&[ENTITY_CODE_MARKER]),
            Err(RecordError::MissingPrefix)
        ));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut bytes = encode(&sample());
        bytes[1] = 0x01; // a version this codec doesn't know
        assert!(matches!(
            decode(&bytes),
            Err(RecordError::UnsupportedVersion(0x01))
        ));
    }

    #[test]
    fn record_codec_implements_entity_codec() {
        let e = sample();
        let codec = RecordCodec;
        assert_eq!(codec.current_version(), RECORD_VERSION);
        assert_eq!(codec.encode(&e), encode(&e));
        assert_eq!(codec.decode(&codec.encode(&e)).unwrap(), e);
        // Default `decode_meta` projects the header.
        let meta = codec.decode_meta(&codec.encode(&e)).unwrap();
        assert_eq!(meta.key, e.key);
        assert_eq!(meta.owner, e.owner);
        assert_eq!(meta.expires_at, e.expires_at);
    }

    #[test]
    fn rejects_truncated_body() {
        let bytes = encode(&sample());
        assert!(matches!(
            decode(&bytes[..bytes.len() - 1]),
            Err(RecordError::Rlp(_))
        ));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode(&sample());
        bytes.push(0xAB);
        assert!(matches!(decode(&bytes), Err(RecordError::TrailingBytes)));
    }
}
