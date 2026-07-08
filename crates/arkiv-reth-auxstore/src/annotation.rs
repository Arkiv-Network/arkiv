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
//! ## Value encoding, and the uint width
//!
//! An indexed value is stored as its raw bytes: a uint as **32 big-endian bytes**,
//! an address as 20, an entity key as 32, a string as its bytes. Notably every
//! uint — a user `ATTR_UINT` attribute *and* the built-in block numbers
//! (`$expiration`, `$createdAtBlock`) — is a full 32-byte word. (The `arkiv-db-engine`
//! reference encoded built-in block numbers as 8 bytes and user uints as 32; that
//! split bought nothing and forced the writer and reader to special-case each field
//! in lockstep, so the port unifies on 32.) Fixed-width big-endian still sorts
//! numerically, so range order is unchanged.

use arkiv_constants::WORD_LEN;
use arkiv_interfaces::entity::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Entity, annotations};
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn};
use arkiv_interfaces::state::AttrEntry;

/// Which physical index an attribute's values live in, beyond the tier-1 equality
/// bitmap every indexed value has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Equality only — a tier-1 pair bitmap, no ordered tier-2 index. For values
    /// with no meaningful order: addresses (`$owner`/`$creator`), the entity key
    /// (`$key`, `ATTR_ENTITY_KEY`), and the `$all` marker.
    Equality,
    /// Tier-2 int-mode B+ tree — numeric range scans. Big-endian uints: the built-in
    /// block numbers (`$expiration`, `$createdAtBlock`) and user `ATTR_UINT` values.
    Int,
    /// Tier-2 str-mode cascade — lexical range and prefix (glob) scans. The
    /// `$contentType` built-in and user `ATTR_STRING` values.
    Str,
}

/// The tier-2 [`Mode`] for `attr` carrying a value of `value_type`.
///
/// Built-in fields have a fixed mode by name; user attributes are classified by
/// their [`ATTR_*`](arkiv_interfaces::entity::ATTR_UINT) tag. Single source of truth
/// for both index maintenance and query evaluation.
pub fn mode_for(attr: &[u8], value_type: u8) -> Mode {
    use annotations::{ALL, CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    match attr {
        ALL | OWNER | CREATOR | KEY => Mode::Equality,
        EXPIRATION | CREATED_AT_BLOCK => Mode::Int,
        CONTENT_TYPE => Mode::Str,
        // A user attribute: classified by its value tag.
        _ => match value_type {
            ATTR_UINT => Mode::Int,
            ATTR_STRING => Mode::Str,
            _ => Mode::Equality, // ATTR_ENTITY_KEY, and anything unrecognized
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

/// The index `value` bytes for a query value — the raw bytes its equality bucket is
/// keyed on. Matches how the writer encodes an entity's value into an
/// [`AttrEntry::value`](arkiv_interfaces::state::AttrEntry); see the module docs on
/// the 32-byte uint width.
pub fn value_bytes(value: &AnnotVal) -> Vec<u8> {
    match value {
        AnnotVal::Uint(word) => word.to_vec(),
        AnnotVal::Str(bytes) => bytes.clone(),
        AnnotVal::Key(key) => key.to_vec(),
        AnnotVal::Addr(addr) => addr.to_vec(),
    }
}

/// The [`ATTR_*`](arkiv_interfaces::entity::ATTR_UINT) tag matching a query value's
/// kind, for [`mode_for`] on the read path. An address is equality-only, like a key.
pub fn value_type(value: &AnnotVal) -> u8 {
    match value {
        AnnotVal::Uint(_) => ATTR_UINT,
        AnnotVal::Str(_) => ATTR_STRING,
        AnnotVal::Key(_) | AnnotVal::Addr(_) => ATTR_ENTITY_KEY,
    }
}

/// A block number as a 32-byte big-endian word — the canonical uint index encoding
/// (see the module docs). Right-aligned so it sorts numerically.
fn block_number_bytes(block: BlockNumber) -> Vec<u8> {
    let mut buf = [0u8; WORD_LEN];
    buf[WORD_LEN - size_of::<BlockNumber>()..].copy_from_slice(&block.to_be_bytes());
    buf.to_vec()
}

/// Every indexable `(attr, value)` pair for an entity: the seven built-ins plus its
/// user attributes. This is the write-side counterpart to the query-side
/// [`attr_bytes`]/[`value_bytes`] — the executor diffs an entity's annotations
/// before and after an op to build a [delta](AttrEntry), so both sides must produce
/// identical bytes, and both live here.
///
/// The built-ins, in order: `$all` (empty value), `$creator`, `$owner`, `$key`,
/// `$createdAtBlock`, `$expiration`, `$contentType`. Addresses are 20 raw bytes, the
/// key 32, block numbers 32-byte big-endian, the content type its raw bytes.
pub fn entity_annotations(entity: &Entity) -> Vec<AttrEntry> {
    use annotations::{ALL, CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER};
    let entry = |attr: &[u8], value_type: u8, value: Vec<u8>| AttrEntry {
        attr: attr.to_vec(),
        value_type,
        value,
    };
    let mut out = vec![
        entry(ALL, ATTR_STRING, Vec::new()),
        entry(CREATOR, ATTR_ENTITY_KEY, entity.creator.to_vec()),
        entry(OWNER, ATTR_ENTITY_KEY, entity.owner.to_vec()),
        entry(KEY, ATTR_ENTITY_KEY, entity.key.to_vec()),
        entry(
            CREATED_AT_BLOCK,
            ATTR_UINT,
            block_number_bytes(entity.created_at_block),
        ),
        entry(EXPIRATION, ATTR_UINT, block_number_bytes(entity.expires_at)),
        entry(CONTENT_TYPE, ATTR_STRING, entity.content_type.clone()),
    ];
    out.extend(entity.attributes.iter().map(|attribute| AttrEntry {
        attr: attribute.key.clone(),
        value_type: attribute.value_type,
        value: attribute.value.clone(),
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_fixed_modes_regardless_of_value_type() {
        // Every built-in's mode is by name; the value_type argument is ignored.
        for vt in [ATTR_UINT, ATTR_STRING, ATTR_ENTITY_KEY, 0] {
            assert_eq!(mode_for(annotations::OWNER, vt), Mode::Equality);
            assert_eq!(mode_for(annotations::CREATOR, vt), Mode::Equality);
            assert_eq!(mode_for(annotations::KEY, vt), Mode::Equality);
            assert_eq!(mode_for(annotations::ALL, vt), Mode::Equality);
            assert_eq!(mode_for(annotations::EXPIRATION, vt), Mode::Int);
            assert_eq!(mode_for(annotations::CREATED_AT_BLOCK, vt), Mode::Int);
            assert_eq!(mode_for(annotations::CONTENT_TYPE, vt), Mode::Str);
        }
    }

    #[test]
    fn user_attrs_are_classified_by_value_type() {
        assert_eq!(mode_for(b"rank", ATTR_UINT), Mode::Int);
        assert_eq!(mode_for(b"name", ATTR_STRING), Mode::Str);
        assert_eq!(mode_for(b"ref", ATTR_ENTITY_KEY), Mode::Equality);
    }

    #[test]
    fn query_value_bytes_match_their_widths() {
        assert_eq!(value_bytes(&AnnotVal::Uint([7u8; 32])).len(), 32);
        assert_eq!(value_bytes(&AnnotVal::Addr([1u8; 20])).len(), 20);
        assert_eq!(value_bytes(&AnnotVal::Key([2u8; 32])).len(), 32);
        assert_eq!(value_bytes(&AnnotVal::Str(b"hi".to_vec())), b"hi");
    }

    #[test]
    fn query_value_type_drives_user_attr_mode() {
        let v = AnnotVal::Uint([0u8; 32]);
        assert_eq!(mode_for(b"rank", value_type(&v)), Mode::Int);
        let v = AnnotVal::Str(b"x".to_vec());
        assert_eq!(mode_for(b"name", value_type(&v)), Mode::Str);
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
            attributes: vec![Attribute {
                key: b"rank".to_vec(),
                value_type: ATTR_UINT,
                value: vec![0u8; 32],
            }],
        };
        let annotations = entity_annotations(&entity);
        // Seven built-ins + one user attribute.
        assert_eq!(annotations.len(), 8);

        // A query against $owner must hit the same (attr, value) bytes the entity's
        // annotation produced — the write/read agreement this module guarantees.
        let owner_query = value_bytes(&AnnotVal::Addr(entity.owner));
        assert!(annotations.iter().any(|a| a.attr == annotations::OWNER
            && a.value == owner_query
            && a.value.len() == 20));

        // $expiration is a 32-byte big-endian uint, matching a Uint query value.
        let exp = annotations
            .iter()
            .find(|a| a.attr == annotations::EXPIRATION)
            .unwrap();
        assert_eq!(exp.value.len(), 32);
        assert_eq!(exp.value[31], 60);
        assert_eq!(exp.value_type, ATTR_UINT);

        // The payload is not indexed.
        assert!(!annotations.iter().any(|a| a.value == b"ignored"));
    }
}
