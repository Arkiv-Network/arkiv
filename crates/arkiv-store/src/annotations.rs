//! Which `(attribute, value)` pairs an entity is indexed under, and the diff
//! between two versions of an entity. The write side (commit) and the read
//! side (query) must agree exactly, so both go through this module.

use arkiv_interfaces::entity::{AttributeType, AttributeValue, Entity, annotations};
use arkiv_interfaces::primitives::{BlockNumber, EntityAddress};
use arkiv_interfaces::query::{AnnotKey, BuiltIn};

/// One `(attribute, value)` pair in the index. The value stays typed because
/// the type picks the index and the ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrEntry {
    pub attr: Vec<u8>,
    pub value: AttributeValue,
}

impl AttrEntry {
    pub fn new(attr: impl Into<Vec<u8>>, value: AttributeValue) -> Self {
        Self {
            attr: attr.into(),
            value,
        }
    }
}

/// One entity's index changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntityDelta {
    pub entity_key: EntityAddress,
    pub inserts: Vec<AttrEntry>,
    pub removes: Vec<AttrEntry>,
}

/// Is a value of `ty` indexed at all? `bytes` (the payload) never is.
pub fn is_indexed(ty: AttributeType) -> bool {
    ty != AttributeType::Bytes
}

/// The index attribute bytes for a query key: the built-in annotation name,
/// or the user attribute's name.
pub fn attr_bytes(key: &AnnotKey) -> Vec<u8> {
    match key {
        AnnotKey::BuiltIn(field) => builtin_attr(*field).to_vec(),
        AnnotKey::User(name) => name.as_bytes().to_vec(),
    }
}

/// The annotation name a built-in field is indexed under. The language's
/// spelling and the stored name are decoupled on purpose: the stored name
/// feeds the index keys, so renaming it is a fork.
pub fn builtin_attr(field: BuiltIn) -> &'static [u8] {
    use annotations::{CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    match field {
        BuiltIn::Owner => OWNER,
        BuiltIn::Creator => CREATOR,
        BuiltIn::Key => KEY,
        BuiltIn::ExpiresAt => EXPIRATION,
        BuiltIn::ContentType => CONTENT_TYPE,
        BuiltIn::CreatedAt => CREATED_AT_BLOCK,
    }
}

/// A block number as an attribute value: a `u256`, so it shares the user
/// `u256` order.
pub fn block_number_value(block: BlockNumber) -> AttributeValue {
    AttributeValue::u256_from_u64(block)
}

/// Every indexable `(attr, value)` pair of an entity: the six built-ins, then
/// the user attributes. There is no `$all` marker: the entity trie itself is
/// the live set.
pub fn entity_annotations(entity: &Entity) -> Vec<AttrEntry> {
    use annotations::{CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    let content_type = String::from_utf8_lossy(&entity.content_type).into_owned();
    let mut out = vec![
        AttrEntry::new(CREATOR, AttributeValue::EthereumAddress(entity.creator)),
        AttrEntry::new(OWNER, AttributeValue::EthereumAddress(entity.owner)),
        AttrEntry::new(KEY, AttributeValue::EntityKey(entity.key)),
        AttrEntry::new(
            CREATED_AT_BLOCK,
            block_number_value(entity.created_at_block),
        ),
        AttrEntry::new(EXPIRATION, block_number_value(entity.expires_at)),
        AttrEntry::new(CONTENT_TYPE, AttributeValue::Str(content_type)),
    ];
    out.extend(
        entity
            .attributes
            .iter()
            .filter(|a| is_indexed(a.value.attr_type()))
            .map(|a| AttrEntry::new(a.key.clone(), a.value.clone())),
    );
    out
}

/// The annotations a `before -> after` transition adds and removes. `None`
/// when nothing changed.
pub fn annotation_delta(
    key: EntityAddress,
    before: Option<&Entity>,
    after: Option<&Entity>,
) -> Option<EntityDelta> {
    let old = before.map(entity_annotations).unwrap_or_default();
    let new = after.map(entity_annotations).unwrap_or_default();
    let removes: Vec<AttrEntry> = old.iter().filter(|e| !new.contains(e)).cloned().collect();
    let inserts: Vec<AttrEntry> = new.iter().filter(|e| !old.contains(e)).cloned().collect();
    if removes.is_empty() && inserts.is_empty() {
        return None;
    }
    Some(EntityDelta {
        entity_key: key,
        inserts,
        removes,
    })
}
