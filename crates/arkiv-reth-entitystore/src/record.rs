//! Entity record codec — an [`Entity`] to and from the bytes an entity account's
//! `code` holds.
//!
//! Format: `0xFE || RLP(entity fields)`, ported byte-for-byte from db-engine's
//! `EntityRlp`. The `0xFE` prefix is the EVM `INVALID` opcode, so a `CALL` to an
//! entity account halts immediately — entities are data, never executed. The RLP
//! **field order is consensus-critical**: it hashes into the state root, so it
//! must not change once state exists.
//!
//! This is host-specific (RLP + account code is reth's storage form), which is why
//! it lives in the entity-store crate, not the spec.

use core::fmt;

use alloy_primitives::{Address, B256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};

use arkiv_interfaces::entity::{Attribute, Entity};

/// Prefix byte before the RLP payload in an entity account's code.
pub const ENTITY_CODE_PREFIX: u8 = 0xFE;

/// Encode an entity to its stored-code bytes: `0xFE || RLP(entity)`.
pub fn encode(entity: &Entity) -> Vec<u8> {
    let rlp = EntityRlp::from_entity(entity);
    let mut out = Vec::with_capacity(1 + rlp.length());
    out.push(ENTITY_CODE_PREFIX);
    rlp.encode(&mut out);
    out
}

/// Decode an entity from its stored-code bytes. Verifies the `0xFE` prefix,
/// RLP-decodes the body, and rejects trailing bytes.
pub fn decode(code: &[u8]) -> Result<Entity, RecordError> {
    if code.first() != Some(&ENTITY_CODE_PREFIX) {
        return Err(RecordError::MissingPrefix);
    }
    let mut body = &code[1..];
    let rlp = EntityRlp::decode(&mut body).map_err(RecordError::Rlp)?;
    if !body.is_empty() {
        return Err(RecordError::TrailingBytes);
    }
    Ok(rlp.into_entity())
}

/// Why decoding an entity record failed.
#[derive(Debug)]
pub enum RecordError {
    /// The code didn't start with the `0xFE` entity prefix.
    MissingPrefix,
    /// The RLP body was malformed.
    Rlp(alloy_rlp::Error),
    /// Extra bytes followed the RLP body.
    TrailingBytes,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::MissingPrefix => write!(f, "entity code is missing the 0xFE prefix"),
            RecordError::Rlp(e) => write!(f, "entity RLP decode failed: {e}"),
            RecordError::TrailingBytes => write!(f, "trailing bytes after the entity record"),
        }
    }
}

impl std::error::Error for RecordError {}

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
        assert_eq!(bytes[0], ENTITY_CODE_PREFIX);
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
