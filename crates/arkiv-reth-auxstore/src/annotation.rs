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

use arkiv_interfaces::entity::annotations;
use arkiv_interfaces::entity::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT};
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn};

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
}
