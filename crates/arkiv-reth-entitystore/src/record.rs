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
use arkiv_interfaces::entity::{
    Attribute, AttributeType, AttributeValue, AttributeValueError, CreationFlags, Entity,
};

/// Marker byte before an entity record — the EVM `INVALID` opcode, so a `CALL` to
/// an entity account halts.
pub const ENTITY_CODE_MARKER: u8 = 0xFE;

/// Current record format version, written right after the marker. Bump it when the
/// on-code layout changes; [`decode`] keeps reading older versions.
///
/// - `0x00` — the original layout.
/// - `0x01` — adds `creation_flags`.
pub const RECORD_VERSION: u8 = 0x01;

/// The original layout, still readable. Records written before creation flags
/// existed decode with all flags clear, which is exactly what they meant.
const RECORD_VERSION_V0: u8 = 0x00;

/// Length of the framing prefix: the marker byte followed by the version byte. The
/// RLP body starts after it.
const PREFIX_LEN: usize = 2;

/// Encode an entity to its stored account-code bytes:
///
/// ```text
///   byte:  0      1                2 ..
///          | 0xFE | RECORD_VERSION | RLP(entity fields) |
/// ```
///
/// `0xFE` is the EVM `INVALID` opcode (a stray `CALL` to an entity account halts);
/// the version byte lets [`decode`] dispatch. Field order is [`EntityRlp`]'s.
pub fn encode(entity: &Entity) -> Vec<u8> {
    let rlp = EntityRlp::from_entity(entity);
    let mut out = Vec::with_capacity(PREFIX_LEN + rlp.length());
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
        RECORD_VERSION => decode_body::<EntityRlp>(&code[PREFIX_LEN..]),
        RECORD_VERSION_V0 => decode_body::<EntityRlpV0>(&code[PREFIX_LEN..]),
        v => Err(RecordError::UnsupportedVersion(v)),
    }
}

/// Decode one version's body and migrate it up to the canonical [`Entity`].
fn decode_body<T: Decodable + IntoEntity>(mut body: &[u8]) -> Result<Entity, RecordError> {
    let rlp = T::decode(&mut body).map_err(RecordError::Rlp)?;
    if !body.is_empty() {
        return Err(RecordError::TrailingBytes);
    }
    rlp.into_entity()
}

/// A versioned on-code layout that can be migrated up to an [`Entity`].
trait IntoEntity {
    fn into_entity(self) -> Result<Entity, RecordError>;
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
    /// An attribute's `typeId` names no type.
    UnknownAttributeType(u8),
    /// An attribute's bytes don't decode as its declared type.
    AttributeValue(AttributeValueError),
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
            RecordError::UnknownAttributeType(t) => {
                write!(f, "unknown attribute value type {t}")
            }
            RecordError::AttributeValue(e) => write!(f, "invalid attribute value: {e}"),
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

/// The entity as it is RLP-encoded into account code (version `0x01`). Field
/// order and types are consensus-critical — do not reorder. `creation_flags` is
/// appended last, so v0 is this layout minus its final field.
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
    creation_flags: u8,
}

/// The version-`0x00` layout: [`EntityRlp`] before creation flags existed.
/// Read-only — nothing writes it any more.
#[derive(RlpEncodable, RlpDecodable)]
struct EntityRlpV0 {
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

impl IntoEntity for EntityRlpV0 {
    /// Migrate up: a record written before flags existed has none set.
    fn into_entity(self) -> Result<Entity, RecordError> {
        EntityRlp {
            payload: self.payload,
            creator: self.creator,
            created_at_block: self.created_at_block,
            owner: self.owner,
            expires_at: self.expires_at,
            content_type: self.content_type,
            key: self.key,
            attributes: self.attributes,
            last_modified_at_block: self.last_modified_at_block,
            creation_flags: 0,
        }
        .into_entity()
    }
}

/// An attribute on-code: its `typeId` byte, then the value's canonical storage
/// bytes ([`AttributeValue::encode`]).
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
            creation_flags: e.creation_flags.bits(),
        }
    }
}

impl IntoEntity for EntityRlp {
    fn into_entity(self) -> Result<Entity, RecordError> {
        Ok(Entity {
            key: self.key.into(),
            creator: self.creator.into(),
            owner: self.owner.into(),
            created_at_block: self.created_at_block,
            last_modified_at_block: self.last_modified_at_block,
            expires_at: self.expires_at,
            creation_flags: CreationFlags::from_stored_bits(self.creation_flags),
            content_type: self.content_type,
            payload: self.payload,
            attributes: self
                .attributes
                .into_iter()
                .map(AttributeRlp::into_attr)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl AttributeRlp {
    fn from_attr(a: &Attribute) -> Self {
        Self {
            key: a.key.clone(),
            value_type: a.value.type_id(),
            value: a.value.encode(),
        }
    }
    fn into_attr(self) -> Result<Attribute, RecordError> {
        let ty = AttributeType::from_id(self.value_type)
            .ok_or(RecordError::UnknownAttributeType(self.value_type))?;
        let value = AttributeValue::decode(ty, &self.value).map_err(RecordError::AttributeValue)?;
        Ok(Attribute {
            key: self.key,
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Entity {
        Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 10,
            last_modified_at_block: 20,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"hello world".to_vec(),
            attributes: vec![
                Attribute::new(b"color".to_vec(), AttributeValue::Str("blue".into())),
                Attribute::new(b"size".to_vec(), AttributeValue::u256_from_u64(7)),
                Attribute::new(b"ref".to_vec(), AttributeValue::EntityKey([9u8; 32])),
                Attribute::new(b"live".to_vec(), AttributeValue::Bool(true)),
                Attribute::new(b"delta".to_vec(), AttributeValue::Int(-3)),
                Attribute::new(b"price".to_vec(), AttributeValue::Decimal([0xFE; 32])),
                Attribute::new(b"hash".to_vec(), AttributeValue::Bytes32([0x5A; 32])),
                Attribute::new(b"who".to_vec(), AttributeValue::EthereumAddress([4u8; 20])),
            ],
        }
    }

    #[test]
    fn roundtrips_full_entity() {
        let e = sample();
        let bytes = encode(&e);
        assert_eq!(&bytes[..PREFIX_LEN], &[ENTITY_CODE_MARKER, RECORD_VERSION]); // 0xFE00
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
        bytes[1] = 0xFF; // a version this codec doesn't know
        assert!(matches!(
            decode(&bytes),
            Err(RecordError::UnsupportedVersion(0xFF))
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
    fn rejects_an_unknown_attribute_type() {
        let rlp = EntityRlp {
            payload: Vec::new(),
            creator: Address::ZERO,
            created_at_block: 0,
            owner: Address::ZERO,
            expires_at: 0,
            content_type: Vec::new(),
            key: B256::ZERO,
            attributes: vec![AttributeRlp {
                key: b"x".to_vec(),
                value_type: 99,
                value: Vec::new(),
            }],
            last_modified_at_block: 0,
            creation_flags: 0,
        };
        let mut body = Vec::new();
        rlp.encode(&mut body);
        assert!(matches!(
            decode_body::<EntityRlp>(&body),
            Err(RecordError::UnknownAttributeType(99))
        ));
    }

    /// A v0 record still decodes, with no flags set — the migration path the
    /// version byte exists for. Written by hand, since nothing emits v0 now.
    #[test]
    fn v0_records_still_decode_with_no_flags() {
        let e = sample();
        let v0 = EntityRlpV0 {
            payload: e.payload.clone(),
            creator: e.creator.into(),
            created_at_block: e.created_at_block,
            owner: e.owner.into(),
            expires_at: e.expires_at,
            content_type: e.content_type.clone(),
            key: e.key.into(),
            attributes: e.attributes.iter().map(AttributeRlp::from_attr).collect(),
            last_modified_at_block: e.last_modified_at_block,
        };
        let mut code = vec![ENTITY_CODE_MARKER, RECORD_VERSION_V0];
        v0.encode(&mut code);

        let decoded = decode(&code).expect("v0 record decodes");
        assert_eq!(decoded.creation_flags, CreationFlags::NONE);
        assert_eq!(decoded.key, e.key);
        assert_eq!(decoded.payload, e.payload);
        assert_eq!(decoded.attributes, e.attributes);
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode(&sample());
        bytes.push(0xAB);
        assert!(matches!(decode(&bytes), Err(RecordError::TrailingBytes)));
    }
}
