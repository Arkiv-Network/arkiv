//! Encoding helpers — Rust types ↔ [`Operation`] / [`Attribute`] calldata.
//!
//! Each op type has a constructor that accepts only the fields relevant to
//! that op and zeros the rest, so callers never need to know the full flat
//! struct layout.
//!
//! # Attributes
//!
//! [`Attribute::from_value`] and [`Attribute::to_value`] are the only place the
//! `AttributeValue` ↔ `bytes32[4]` wire mapping is written down, in both
//! directions. The contract requires attributes sorted ascending by name;
//! [`Operation::create`] and [`Operation::update`] sort automatically.

use core::fmt;

use alloy_primitives::{Address, B256, Bytes, FixedBytes};
use arkiv_interfaces::entity::{AttributeType, AttributeValue};

use crate::{
    Attribute, Ident32, Mime128, OP_CREATE, OP_DELETE, OP_EXPIRE, OP_EXTEND, OP_TRANSFER,
    OP_UPDATE, Operation,
};

/// The longest `string` value: the four words of `Attribute.value`.
pub const MAX_STRING_BYTES: usize = 4 * WORD;

const WORD: usize = 32;

// -----------------------------------------------------------------------------
// Operation constructors
// -----------------------------------------------------------------------------

impl Operation {
    /// Create a new entity.
    ///
    /// `btl` is blocks-to-live from the current block
    /// (`expiresAt = currentBlock + btl`). Attributes are sorted by name
    /// ascending automatically.
    pub fn create(
        btl: u32,
        payload: Bytes,
        content_type: Mime128,
        mut attributes: Vec<Attribute>,
    ) -> Self {
        Attribute::sort(&mut attributes);
        Self {
            operationType: OP_CREATE,
            entityKey: B256::ZERO,
            payload,
            contentType: content_type,
            attributes,
            btl,
            newOwner: Address::ZERO,
        }
    }

    /// Update an existing entity's payload, content type, and attributes.
    ///
    /// Attributes are sorted by name ascending automatically.
    pub fn update(
        entity_key: B256,
        payload: Bytes,
        content_type: Mime128,
        mut attributes: Vec<Attribute>,
    ) -> Self {
        Attribute::sort(&mut attributes);
        Self {
            operationType: OP_UPDATE,
            entityKey: entity_key,
            payload,
            contentType: content_type,
            attributes,
            ..Default::default()
        }
    }

    /// Extend an entity's expiry by `btl` blocks from the current block.
    pub fn extend(entity_key: B256, btl: u32) -> Self {
        Self {
            operationType: OP_EXTEND,
            entityKey: entity_key,
            btl,
            ..Default::default()
        }
    }

    /// Transfer entity ownership.
    pub fn transfer(entity_key: B256, new_owner: Address) -> Self {
        Self {
            operationType: OP_TRANSFER,
            entityKey: entity_key,
            newOwner: new_owner,
            ..Default::default()
        }
    }

    /// Delete an entity before its expiry.
    pub fn delete(entity_key: B256) -> Self {
        Self {
            operationType: OP_DELETE,
            entityKey: entity_key,
            ..Default::default()
        }
    }

    /// Remove an expired entity from storage.
    pub fn expire(entity_key: B256) -> Self {
        Self {
            operationType: OP_EXPIRE,
            entityKey: entity_key,
            ..Default::default()
        }
    }
}

// -----------------------------------------------------------------------------
// Attribute constructors
// -----------------------------------------------------------------------------

impl Attribute {
    /// The ABI attribute carrying `value` under `name`.
    ///
    /// Single-word types occupy `value[0]` in their standard ABI encoding
    /// (right-aligned and sign-extended where Solidity would); a `string` is
    /// left-aligned across up to all four words.
    pub fn from_value(name: Ident32, value: &AttributeValue) -> Result<Self, AttrAbiError> {
        let mut words = [FixedBytes::ZERO; 4];
        match value {
            AttributeValue::Bool(b) => words[0].0[WORD - 1] = u8::from(*b),
            AttributeValue::Int(n) => {
                let mut w = [if *n < 0 { 0xFF } else { 0x00 }; WORD];
                w[WORD - 4..].copy_from_slice(&n.to_be_bytes());
                words[0] = FixedBytes::from(w);
            }
            AttributeValue::U64(n) => words[0].0[WORD - 8..].copy_from_slice(&n.to_be_bytes()),
            AttributeValue::U256(w)
            | AttributeValue::Decimal(w)
            | AttributeValue::Bytes32(w)
            | AttributeValue::EntityKey(w) => words[0] = FixedBytes::from(*w),
            AttributeValue::EthereumAddress(a) => words[0].0[WORD - 20..].copy_from_slice(a),
            AttributeValue::Str(s) => {
                if s.len() > MAX_STRING_BYTES {
                    return Err(AttrAbiError::StringTooLong(s.len()));
                }
                for (i, chunk) in s.as_bytes().chunks(WORD).enumerate() {
                    words[i].0[..chunk.len()].copy_from_slice(chunk);
                }
            }
            AttributeValue::Bytes(_) => return Err(AttrAbiError::SystemOnlyType),
        }
        Ok(Self {
            name: name.0,
            valueType: value.type_id(),
            value: words,
        })
    }

    /// Read the attribute's value back, rejecting anything a well-formed ABI
    /// encoder would not have produced.
    pub fn to_value(&self) -> Result<AttributeValue, AttrAbiError> {
        let ty = self.wire_type()?;
        match ty {
            // A string spans the whole word array; every other type is one word,
            // so the rest must be untouched.
            AttributeType::Str => decode_string(&self.value),
            _ => {
                self.reject_spilled_words()?;
                decode_word(ty, self.value[0].0)
            }
        }
    }

    /// The declared type, if a client is allowed to send it at all.
    fn wire_type(&self) -> Result<AttributeType, AttrAbiError> {
        let ty = AttributeType::from_id(self.valueType)
            .ok_or(AttrAbiError::UnknownType(self.valueType))?;
        if ty.is_user_settable() {
            Ok(ty)
        } else {
            Err(AttrAbiError::SystemOnlyType)
        }
    }

    /// Reject a single-word value carrying data in the words after the first.
    fn reject_spilled_words(&self) -> Result<(), AttrAbiError> {
        match self.value[1..].iter().position(|w| *w != FixedBytes::ZERO) {
            Some(i) => Err(AttrAbiError::NonZeroPadding { word: i + 1 }),
            None => Ok(()),
        }
    }

    /// Sort attributes by name ascending.
    ///
    /// The contract requires strict ascending order for deterministic hashing
    /// and to enforce name uniqueness. Called automatically by
    /// [`Operation::create`] and [`Operation::update`].
    pub fn sort(attrs: &mut [Self]) {
        attrs.sort_by_key(|a| a.name);
    }
}

/// Decode a one-word value of type `ty` from its ABI word.
fn decode_word(ty: AttributeType, w: [u8; WORD]) -> Result<AttributeValue, AttrAbiError> {
    match ty {
        AttributeType::Bool => decode_bool(w),
        AttributeType::Int => decode_int(w),
        AttributeType::EthereumAddress => {
            zero_prefix(&w, WORD - 20)?;
            Ok(AttributeValue::EthereumAddress(
                w[WORD - 20..].try_into().unwrap(),
            ))
        }
        AttributeType::U64 => {
            zero_prefix(&w, WORD - 8)?;
            Ok(AttributeValue::U64(u64::from_be_bytes(
                w[WORD - 8..].try_into().unwrap(),
            )))
        }
        AttributeType::U256 => Ok(AttributeValue::U256(w)),
        AttributeType::Decimal => Ok(AttributeValue::Decimal(w)),
        AttributeType::Bytes32 => Ok(AttributeValue::Bytes32(w)),
        AttributeType::EntityKey => Ok(AttributeValue::EntityKey(w)),
        // Neither reaches here: `str` is handled by the caller, and `bytes` is
        // rejected by `wire_type`.
        AttributeType::Str | AttributeType::Bytes => Err(AttrAbiError::SystemOnlyType),
    }
}

/// An ABI `bool`: right-aligned, and only 0 or 1.
fn decode_bool(w: [u8; WORD]) -> Result<AttributeValue, AttrAbiError> {
    zero_prefix(&w, WORD - 1)?;
    match w[WORD - 1] {
        0 => Ok(AttributeValue::Bool(false)),
        1 => Ok(AttributeValue::Bool(true)),
        other => Err(AttrAbiError::BadBool(other)),
    }
}

/// An ABI `int32`: right-aligned, with the leading bytes sign-extended from the
/// value's top bit.
fn decode_int(w: [u8; WORD]) -> Result<AttributeValue, AttrAbiError> {
    let fill = if w[WORD - 4] & 0x80 == 0 { 0x00 } else { 0xFF };
    if w[..WORD - 4].iter().any(|b| *b != fill) {
        return Err(AttrAbiError::BadSignExtension);
    }
    Ok(AttributeValue::Int(i32::from_be_bytes(
        w[WORD - 4..].try_into().unwrap(),
    )))
}

/// An ABI `string`: the words packed back together, trailing padding dropped.
fn decode_string(words: &[FixedBytes<32>; 4]) -> Result<AttributeValue, AttrAbiError> {
    String::from_utf8(pack_words(words))
        .map(AttributeValue::Str)
        .map_err(|_| AttrAbiError::NotUtf8)
}

/// The four words concatenated, with trailing zero padding removed — how a
/// `string` or a [`Mime128`] is read back out of its fixed-size word array.
pub fn pack_words(words: &[FixedBytes<32>; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 * WORD);
    for w in words {
        out.extend_from_slice(w.as_slice());
    }
    while matches!(out.last(), Some(0)) {
        out.pop();
    }
    out
}

/// Check that a right-aligned value's leading `len` bytes are zero padding.
fn zero_prefix(word: &[u8; WORD], len: usize) -> Result<(), AttrAbiError> {
    if word[..len].iter().any(|b| *b != 0) {
        Err(AttrAbiError::NonZeroValuePadding)
    } else {
        Ok(())
    }
}

/// Why an attribute value didn't survive the ABI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrAbiError {
    /// The `valueType` byte names no type.
    UnknownType(u8),
    /// `bytes` — system-only, never carried in an attribute array.
    SystemOnlyType,
    /// A single-word value carried data past its first word.
    NonZeroPadding { word: usize },
    /// A right-aligned value's leading padding wasn't zero.
    NonZeroValuePadding,
    /// A `bool` word held something other than 0 or 1.
    BadBool(u8),
    /// An `int32` wasn't sign-extended across its word.
    BadSignExtension,
    /// A `string` value wasn't valid UTF-8.
    NotUtf8,
    /// A `string` value exceeded [`MAX_STRING_BYTES`].
    StringTooLong(usize),
}

impl fmt::Display for AttrAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownType(t) => write!(f, "unknown attribute value type {t}"),
            Self::SystemOnlyType => write!(f, "attribute value type is system-only"),
            Self::NonZeroPadding { word } => {
                write!(f, "attribute value has data past word 0 (word {word})")
            }
            Self::NonZeroValuePadding => write!(f, "attribute value has non-zero leading padding"),
            Self::BadBool(b) => write!(f, "bool value must be 0 or 1, got {b}"),
            Self::BadSignExtension => write!(f, "int value is not sign-extended"),
            Self::NotUtf8 => write!(f, "string value is not valid UTF-8"),
            Self::StringTooLong(n) => {
                write!(f, "string value exceeds {MAX_STRING_BYTES} bytes ({n})")
            }
        }
    }
}

impl std::error::Error for AttrAbiError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Ident32 {
        Ident32::encode(s).unwrap()
    }

    // -------------------------------------------------------------------------
    // Operation constructors
    // -------------------------------------------------------------------------

    #[test]
    fn create_sets_op_type_and_zeroes_entity_key() {
        let op = Operation::create(100, Bytes::new(), Mime128::default(), vec![]);
        assert_eq!(op.operationType, OP_CREATE);
        assert_eq!(op.entityKey, B256::ZERO);
        assert_eq!(op.btl, 100);
        assert_eq!(op.newOwner, Address::ZERO);
    }

    fn u256(name_: &str, n: u64) -> Attribute {
        Attribute::from_value(name(name_), &AttributeValue::u256_from_u64(n)).unwrap()
    }

    #[test]
    fn create_sorts_attributes() {
        let attrs = vec![u256("z.attr", 1), u256("a.attr", 2)];
        let op = Operation::create(100, Bytes::new(), Mime128::default(), attrs);
        // a.attr < z.attr lexicographically
        assert!(op.attributes[0].name < op.attributes[1].name);
    }

    #[test]
    fn update_sets_op_type() {
        let op = Operation::update(
            B256::repeat_byte(1),
            Bytes::new(),
            Mime128::default(),
            vec![],
        );
        assert_eq!(op.operationType, OP_UPDATE);
        assert_eq!(op.entityKey, B256::repeat_byte(1));
        assert_eq!(op.btl, 0);
    }

    #[test]
    fn extend_sets_btl() {
        let key = B256::repeat_byte(2);
        let op = Operation::extend(key, 500);
        assert_eq!(op.operationType, OP_EXTEND);
        assert_eq!(op.entityKey, key);
        assert_eq!(op.btl, 500);
    }

    #[test]
    fn transfer_sets_new_owner() {
        let key = B256::repeat_byte(3);
        let owner = Address::repeat_byte(0xAB);
        let op = Operation::transfer(key, owner);
        assert_eq!(op.operationType, OP_TRANSFER);
        assert_eq!(op.newOwner, owner);
    }

    #[test]
    fn delete_and_expire_set_op_types() {
        let key = B256::repeat_byte(4);
        assert_eq!(Operation::delete(key).operationType, OP_DELETE);
        assert_eq!(Operation::expire(key).operationType, OP_EXPIRE);
    }

    // -------------------------------------------------------------------------
    // Attribute constructors
    // -------------------------------------------------------------------------

    /// Every type survives the round trip through its ABI words.
    #[test]
    fn values_round_trip_through_the_wire() {
        let values = [
            AttributeValue::Bool(true),
            AttributeValue::Bool(false),
            AttributeValue::Int(-42),
            AttributeValue::Int(i32::MIN),
            AttributeValue::Int(i32::MAX),
            AttributeValue::u256_from_u64(42),
            AttributeValue::Decimal([0xFF; 32]),
            AttributeValue::Bytes32([0x5A; 32]),
            AttributeValue::Str("hello".into()),
            AttributeValue::Str("é".repeat(64)), // 128 bytes, the maximum
            AttributeValue::EthereumAddress([0x11; 20]),
            AttributeValue::EntityKey([0x77; 32]),
        ];
        for value in values {
            let attr = Attribute::from_value(name("a"), &value).unwrap();
            assert_eq!(attr.valueType, value.type_id());
            assert_eq!(attr.to_value().unwrap(), value, "{value:?}");
        }
    }

    #[test]
    fn single_word_types_leave_the_upper_words_zero() {
        let attr =
            Attribute::from_value(name("count"), &AttributeValue::u256_from_u64(42)).unwrap();
        assert_eq!(attr.value[0].0[31], 42);
        assert!(attr.value[1..].iter().all(|w| *w == FixedBytes::ZERO));
    }

    #[test]
    fn string_packs_left_aligned() {
        let attr = Attribute::from_value(name("title"), &AttributeValue::Str("hi".into())).unwrap();
        assert_eq!(&attr.value[0].0[..2], b"hi");
        assert_eq!(attr.value[1], FixedBytes::ZERO);
    }

    #[test]
    fn rejects_oversized_and_system_only_values() {
        let long = AttributeValue::Str("x".repeat(MAX_STRING_BYTES + 1));
        assert_eq!(
            Attribute::from_value(name("big"), &long),
            Err(AttrAbiError::StringTooLong(MAX_STRING_BYTES + 1))
        );
        assert!(
            Attribute::from_value(
                name("full"),
                &AttributeValue::Str("x".repeat(MAX_STRING_BYTES))
            )
            .is_ok()
        );
        assert_eq!(
            Attribute::from_value(name("p"), &AttributeValue::Bytes(vec![1])),
            Err(AttrAbiError::SystemOnlyType)
        );
    }

    /// Malformed words a hand-rolled encoder could produce are rejected, not
    /// silently truncated.
    #[test]
    fn to_value_rejects_malformed_words() {
        let with = |ty: AttributeType, words: [FixedBytes<32>; 4]| Attribute {
            name: name("a").0,
            valueType: ty.id(),
            value: words,
        };
        let word = |bytes: [u8; 32]| {
            let mut w = [FixedBytes::ZERO; 4];
            w[0] = FixedBytes::from(bytes);
            w
        };

        let mut two = [0u8; 32];
        two[31] = 2;
        assert_eq!(
            with(AttributeType::Bool, word(two)).to_value(),
            Err(AttrAbiError::BadBool(2))
        );

        // An int32 whose upper bytes don't match its sign bit.
        let mut bad_int = [0u8; 32];
        bad_int[0] = 0xFF;
        assert_eq!(
            with(AttributeType::Int, word(bad_int)).to_value(),
            Err(AttrAbiError::BadSignExtension)
        );

        // An address with junk in its leading padding.
        assert_eq!(
            with(AttributeType::EthereumAddress, word([0xAB; 32])).to_value(),
            Err(AttrAbiError::NonZeroValuePadding)
        );

        // A single-word type carrying data in a later word.
        let mut spilled = [FixedBytes::ZERO; 4];
        spilled[2] = FixedBytes::from([1u8; 32]);
        assert_eq!(
            with(AttributeType::U256, spilled).to_value(),
            Err(AttrAbiError::NonZeroPadding { word: 2 })
        );

        assert_eq!(
            with(AttributeType::Str, word([0xFF; 32])).to_value(),
            Err(AttrAbiError::NotUtf8)
        );

        let unknown = Attribute {
            name: name("a").0,
            valueType: 99,
            value: [FixedBytes::ZERO; 4],
        };
        assert_eq!(unknown.to_value(), Err(AttrAbiError::UnknownType(99)));
    }

    #[test]
    fn sort_orders_by_name_ascending() {
        let mut attrs = vec![u256("z.last", 0), u256("a.first", 0), u256("m.mid", 0)];
        Attribute::sort(&mut attrs);
        assert!(attrs[0].name < attrs[1].name);
        assert!(attrs[1].name < attrs[2].name);
    }
}
