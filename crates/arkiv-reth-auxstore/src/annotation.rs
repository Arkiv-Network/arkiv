//! Shared annotation classification: which physical index an attribute uses, and
//! how a query key/value maps to the index bytes.
//!
//! The write path ([`apply_delta`](crate::store::RethAuxStore)) and the read path
//! ([`evaluate`](crate::store::RethAuxStore)) must agree here *exactly*: a query
//! value has to hash to the same [`pair_address`](crate::address::pair_address) as
//! the stored value, and a range predicate has to consult the same tier-2 structure
//! (int B+ tree vs str cascade) the writer maintained. Both sides go through this
//! one module so they cannot drift.
//!
//! ## Value encoding
//!
//! An indexed value is keyed on its [`AttributeValue::index_bytes`]: an address as
//! 20 bytes, an entity key as 32, a string as its UTF-8 bytes, a `u256` as a
//! 32-byte big-endian word, a signed `int`/`decimal` as its sign-bit-biased bytes
//! (so byte order is numeric order). Built-in block numbers (`$expiration`,
//! `$createdAtBlock`) are plain `u256`s, sharing the user `u256` encoding.
//!
//! The value's *type* is part of the index key too (see
//! [`address`](crate::address)), so one attribute name holding two types keeps two
//! disjoint sets of buckets and one ordered structure per type.

use arkiv_interfaces::entity::{AttributeType, AttributeValue, Entity, annotations};
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::query::{AnnotKey, BuiltIn};
use arkiv_interfaces::state::AttrEntry;

/// What an attribute can be queried by — the spec's indexing column, and with it
/// the physical structures the value is recorded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCapabilities {
    /// Not indexed — `bytes`, the system-only `$payload` type.
    None,
    /// Equality only: a tier-1 pair bitmap, no ordered tier-2 index. For values
    /// with no useful order — booleans, `bytes32`, addresses (`$owner`,
    /// `$creator`), entity keys (`$key`) — and the `$all` marker.
    Equality,
    /// Equality + range: numerically ordered, so `<`/`>` scan the tier-2 B+ tree.
    /// The numeric types (`int`, `u256`, `decimal`) and the built-in block numbers.
    EqualityAndRange,
    /// Equality + prefix: lexically ordered, so `<`/`>` and glob scan the tier-2
    /// cascade. Strings, and the `$contentType` built-in.
    EqualityAndPrefix,
}

/// What `attr` can be queried by when it carries a value of type `ty`.
///
/// Built-in fields are fixed by name; user attributes are classified by their
/// type. Single source of truth for both index maintenance and query evaluation.
pub fn capabilities_for(attr: &[u8], ty: AttributeType) -> QueryCapabilities {
    use annotations::{ALL, CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    match attr {
        ALL | OWNER | CREATOR | KEY => QueryCapabilities::Equality,
        EXPIRATION | CREATED_AT_BLOCK => QueryCapabilities::EqualityAndRange,
        CONTENT_TYPE => QueryCapabilities::EqualityAndPrefix,
        _ => match ty {
            AttributeType::Int | AttributeType::U256 | AttributeType::Decimal => {
                QueryCapabilities::EqualityAndRange
            }
            AttributeType::Str => QueryCapabilities::EqualityAndPrefix,
            AttributeType::Bytes => QueryCapabilities::None,
            AttributeType::Bool
            | AttributeType::Bytes32
            | AttributeType::EthereumAddress
            | AttributeType::EntityKey => QueryCapabilities::Equality,
        },
    }
}

/// The index `attr` bytes for a query key: the built-in annotation name, or the
/// user attribute's name bytes.
pub fn attr_bytes(key: &AnnotKey) -> Vec<u8> {
    match key {
        AnnotKey::BuiltIn(field) => builtin_attr(*field).to_vec(),
        AnnotKey::User(name) => name.as_bytes().to_vec(),
    }
}

/// The built-in annotation name for a [`BuiltIn`] field — the `attr` its pair
/// buckets are keyed under.
pub fn builtin_attr(field: BuiltIn) -> &'static [u8] {
    use annotations::{CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    match field {
        BuiltIn::Owner => OWNER,
        BuiltIn::Creator => CREATOR,
        BuiltIn::Key => KEY,
        BuiltIn::Expiration => EXPIRATION,
        BuiltIn::ContentType => CONTENT_TYPE,
        BuiltIn::CreatedAtBlock => CREATED_AT_BLOCK,
    }
}

/// A block number as an attribute value: a right-aligned 32-byte big-endian word,
/// so it sorts numerically alongside user `u256`s.
fn block_number_value(block: BlockNumber) -> AttributeValue {
    AttributeValue::u256_from_u64(block)
}

/// Every indexable `(attr, value)` pair for an entity: the seven built-ins plus its
/// user attributes. This is the write-side counterpart to the query-side
/// [`attr_bytes`]/[`value_bytes`] — the executor diffs an entity's annotations
/// before and after an op to build a [delta](AttrEntry), so both sides must produce
/// identical bytes, and both live here.
///
/// The built-ins, in order: `$all` (empty value), `$creator`, `$owner`, `$key`,
/// `$createdAtBlock`, `$expiration`, `$contentType`.
pub fn entity_annotations(entity: &Entity) -> Vec<AttrEntry> {
    use annotations::{ALL, CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    // The content type is a validated MIME string upstream, so the lossy conversion
    // is a no-op in practice.
    let content_type = String::from_utf8_lossy(&entity.content_type).into_owned();
    let mut out = vec![
        AttrEntry::new(ALL, AttributeValue::Str(String::new())),
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
            .map(|a| AttrEntry::new(a.key.clone(), a.value.clone())),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_fixed_capabilities_regardless_of_value_type() {
        // Every built-in is fixed by name; the type argument is ignored.
        for id in 1..=9u8 {
            let ty = AttributeType::from_id(id).unwrap();
            assert_eq!(
                capabilities_for(annotations::OWNER, ty),
                QueryCapabilities::Equality
            );
            assert_eq!(
                capabilities_for(annotations::CREATOR, ty),
                QueryCapabilities::Equality
            );
            assert_eq!(
                capabilities_for(annotations::KEY, ty),
                QueryCapabilities::Equality
            );
            assert_eq!(
                capabilities_for(annotations::ALL, ty),
                QueryCapabilities::Equality
            );
            assert_eq!(
                capabilities_for(annotations::EXPIRATION, ty),
                QueryCapabilities::EqualityAndRange
            );
            assert_eq!(
                capabilities_for(annotations::CREATED_AT_BLOCK, ty),
                QueryCapabilities::EqualityAndRange
            );
            assert_eq!(
                capabilities_for(annotations::CONTENT_TYPE, ty),
                QueryCapabilities::EqualityAndPrefix
            );
        }
    }

    /// The spec's indexing column, type by type.
    #[test]
    fn user_attrs_are_classified_by_type() {
        let expected = [
            (AttributeType::Bool, QueryCapabilities::Equality),
            (AttributeType::Int, QueryCapabilities::EqualityAndRange),
            (AttributeType::U256, QueryCapabilities::EqualityAndRange),
            (AttributeType::Decimal, QueryCapabilities::EqualityAndRange),
            (AttributeType::Bytes32, QueryCapabilities::Equality),
            (AttributeType::Bytes, QueryCapabilities::None),
            (AttributeType::Str, QueryCapabilities::EqualityAndPrefix),
            (AttributeType::EthereumAddress, QueryCapabilities::Equality),
            (AttributeType::EntityKey, QueryCapabilities::Equality),
        ];
        for (ty, expect) in expected {
            assert_eq!(capabilities_for(b"user", ty), expect, "{ty:?}");
        }
    }

    #[test]
    fn entity_annotations_covers_builtins_plus_user_attrs() {
        use arkiv_interfaces::entity::Attribute;
        let entity = Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 3,
            last_modified_at_block: 4,
            expires_at: 60,
            content_type: b"text/plain".to_vec(),
            payload: b"ignored".to_vec(),
            attributes: vec![Attribute::new(
                b"rank".to_vec(),
                AttributeValue::u256_from_u64(5),
            )],
        };
        let entries = entity_annotations(&entity);
        // Seven built-ins + one user attribute.
        assert_eq!(entries.len(), 8);

        // A query against $owner must produce the same value the entity's annotation
        // did — the write/read agreement this module guarantees.
        let owner = entries
            .iter()
            .find(|a| a.attr == annotations::OWNER)
            .unwrap();
        assert_eq!(owner.value, AttributeValue::EthereumAddress(entity.owner));
        assert_eq!(owner.value.index_bytes().len(), 20);

        // $expiration is a u256, so it shares the user u256 encoding and ordering.
        let exp = entries
            .iter()
            .find(|a| a.attr == annotations::EXPIRATION)
            .unwrap();
        assert_eq!(exp.value, AttributeValue::u256_from_u64(60));

        // The payload is not indexed.
        assert!(!entries.iter().any(|a| a.value.encode() == b"ignored"));
    }
}
