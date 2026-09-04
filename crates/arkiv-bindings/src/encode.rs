//! Encoding helpers — Rust types ↔ [`Operation`] / [`Attribute`] calldata.
//!
//! # Operations
//!
//! Each op has a constructor that takes only its own fields and ABI-encodes them
//! into the [`Operation::operationData`] blob, so callers never assemble the tagged union by hand.
//!
//! [`Operation::payload_of`] is the inverse, and rejects a blob that is not the **canonical** encoding
//! of its struct — a spec requirement (`arkiv-node-api.md` §3), and the reason decode compares
//! against a re-encode rather than trusting the decoder to be strict.
//!
//! # Attributes
//!
//! [`Attribute::from_value`] and [`Attribute::to_value`] are the only place the
//! `AttributeValue` ↔ wire mapping is written down, in both directions. The
//! encoding is selected by `typeId`:
//!
//! | typeId            | wire bytes                              |
//! |-------------------|-----------------------------------------|
//! | `0` (tombstone)   | zero-length                             |
//! | word types        | ABI encoded data, exactly one 32 bytes  |
//! | `string`, `bytes` | raw bytes, unpadded                     |
//!
//! See the [Contract ABI Specification](https://docs.soliditylang.org/en/latest/abi-spec.html) for more details.
//!
//! Where word types can be any of the following below.
//!
//! -> Padded with trailing zero-bytes to a length of 32 bytes
//!     - `bool`
//!     - `int` (sign-extended)
//!     - `u64`
//!     - `u256`
//!     - `decimal`
//!     - `address`
//! -> Full-width 32-byte values occupy the entire word.
//!     - `bytes32`
//!     - `entity_key`
//!
//! Attributes must be sorted strictly ascending by name; the [`Operation`] constructors sort them automatically.

use core::fmt;

use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::SolValue;
use arkiv_interfaces::constants::ethereum::{ETH_ADDRESS_LEN, EVM_WORD_LENGTH};
use arkiv_interfaces::entity::{AttributeType, AttributeValue, TOMBSTONE_TYPE_ID};

use crate::{
    Attribute, Create, Delete, ExtendExpiry, Ident32, OP_CREATE, OP_DELETE, OP_EXTEND_EXPIRY,
    OP_PATCH, OP_TRANSFER_OWNERSHIP, Operation, Patch, TransferOwnership,
};

/// The protocol's value limits — `arkiv-engine.md` §2. Re-exported rather than
/// restated so the check this crate runs before a transaction is sent is the same
/// number the engine applies on decode.
pub use arkiv_interfaces::constants::{MAX_PAYLOAD_BYTES, MAX_STR_BYTES};

// -----------------------------------------------------------------------------
// Operation constructors
// -----------------------------------------------------------------------------

impl Operation {
    /// Create a new entity.
    ///
    /// Expiry resolves to `max(expires_at, current_block + min_lifetime)`, so
    /// `expires_at = 0` is a pure relative lifetime and `min_lifetime = 0` a
    /// pure absolute one. `salt` only buys unpredictability — the per-owner
    /// nonce is what guarantees uniqueness — so `0` mints a valid but publicly
    /// predictable key; SDKs should default it to 128 random bits.
    ///
    /// Attributes are sorted by name ascending automatically. Tombstones are
    /// not valid here (nothing exists yet to unset) and are rejected at decode.
    pub fn create(
        salt: u128,
        expires_at: u64,
        min_lifetime: u64,
        creation_flags: u8,
        mut attributes: Vec<Attribute>,
    ) -> Self {
        Attribute::sort(&mut attributes);
        Self::tagged(
            OP_CREATE,
            Create {
                salt,
                expiresAt: expires_at,
                minLifetime: min_lifetime,
                creationFlags: creation_flags,
                attributes,
            },
        )
    }

    /// Partially mutate an entity: each triple sets an attribute, or unsets one
    /// via [`Attribute::tombstone`]. Untouched attributes are left alone —
    /// unlike a whole-entity replace, so concurrent patches of disjoint
    /// attributes compose instead of clobbering each other.
    ///
    /// Mutations are sorted by name ascending automatically.
    pub fn patch(entity_key: B256, mut mutations: Vec<Attribute>) -> Self {
        Attribute::sort(&mut mutations);
        Self::tagged(
            OP_PATCH,
            Patch {
                entityKey: entity_key,
                mutations,
            },
        )
    }

    /// Move an entity's expiry further out. Resolved as in [`create`](Self::create),
    /// then required to be at or past the current expiry: later extends, equal
    /// is a no-op, earlier reverts — lifetimes never shorten.
    pub fn extend_expiry(entity_key: B256, expires_at: u64, min_lifetime: u64) -> Self {
        Self::tagged(
            OP_EXTEND_EXPIRY,
            ExtendExpiry {
                entityKey: entity_key,
                expiresAt: expires_at,
                minLifetime: min_lifetime,
            },
        )
    }

    /// Transfer entity ownership.
    pub fn transfer_ownership(entity_key: B256, new_owner: Address) -> Self {
        Self::tagged(
            OP_TRANSFER_OWNERSHIP,
            TransferOwnership {
                entityKey: entity_key,
                newOwner: new_owner,
            },
        )
    }

    /// Delete an entity before its expiry.
    pub fn delete(entity_key: B256) -> Self {
        Self::tagged(
            OP_DELETE,
            Delete {
                entityKey: entity_key,
            },
        )
    }

    /// Wrap an ABI-encodable payload struct under its `operation` tag.
    fn tagged(operation: u8, payload: impl SolValue) -> Self {
        Self {
            operation,
            operationData: Bytes::from(payload.abi_encode()),
        }
    }

    /// Decode this op's `operationData` as `T`, rejecting a non-canonical
    /// encoding.
    ///
    /// The caller picks `T` from the [`operation`](Self::operation) tag. A
    /// decoder alone is not enough: ABI decoding tolerates blobs a canonical
    /// encoder would never emit (dirty padding, oversized offsets, trailing
    /// junk), and two spellings of one op must not both be valid. So the
    /// decoded value is **re-encoded and compared** — the encoder defines
    /// canonical, and anything that does not round-trip is rejected.
    pub fn payload_of<T: SolValue + From<<T::SolType as alloy_sol_types::SolType>::RustType>>(
        &self,
    ) -> Result<T, OpAbiError> {
        let decoded = T::abi_decode(&self.operationData)
            .map_err(|e| OpAbiError::Abi(alloc_string(&e.to_string())))?;
        if decoded.abi_encode() != self.operationData.as_ref() {
            return Err(OpAbiError::NonCanonical);
        }
        Ok(decoded)
    }
}

fn alloc_string(s: &str) -> String {
    s.to_string()
}

// -----------------------------------------------------------------------------
// Attribute constructors
// -----------------------------------------------------------------------------

impl Attribute {
    /// The ABI attribute carrying `value` under `name`.
    pub fn from_value(name: Ident32, value: &AttributeValue) -> Result<Self, AttrAbiError> {
        let bytes = encode_value(value)?;
        Ok(Self {
            name: name.0,
            typeId: value.type_id(),
            value: Bytes::from(bytes),
        })
    }

    /// A **tombstone**: unset `name`. Valid only in a `patch` mutation list —
    /// on a fresh entity "absent" is said by omission, so a tombstone in a
    /// `create` is a revert.
    pub fn tombstone(name: Ident32) -> Self {
        Self {
            name: name.0,
            typeId: TOMBSTONE_TYPE_ID,
            value: Bytes::new(),
        }
    }

    /// Whether this triple unsets its attribute rather than setting it.
    pub fn is_tombstone(&self) -> bool {
        self.typeId == TOMBSTONE_TYPE_ID
    }

    /// Read the value back, rejecting anything a canonical encoder would not
    /// have produced. `Ok(None)` is a tombstone.
    pub fn to_value(&self) -> Result<Option<AttributeValue>, AttrAbiError> {
        if self.is_tombstone() {
            // Canonical encoding: under typeId 0 the only valid value is the
            // empty byte string, so a tombstone has exactly one spelling.
            return if self.value.is_empty() {
                Ok(None)
            } else {
                Err(AttrAbiError::TombstoneNotEmpty(self.value.len()))
            };
        }
        let ty =
            AttributeType::from_id(self.typeId).ok_or(AttrAbiError::UnknownType(self.typeId))?;
        decode_value(ty, &self.value).map(Some)
    }

    /// Sort attributes by name ascending.
    ///
    /// The protocol requires strictly ascending order — it makes the encoding
    /// canonical and enforces name uniqueness in one rule. Called
    /// automatically by the [`Operation`] constructors.
    pub fn sort(attrs: &mut [Self]) {
        attrs.sort_by_key(|a| a.name);
    }
}

/// Encode a value to its wire bytes: one right-aligned word for the word
/// types, raw bytes for `string`/`bytes`.
fn encode_value(value: &AttributeValue) -> Result<Vec<u8>, AttrAbiError> {
    let word = |fill: u8, tail: &[u8]| {
        let mut w = [fill; EVM_WORD_LENGTH];
        w[EVM_WORD_LENGTH - tail.len()..].copy_from_slice(tail);
        Vec::from(w)
    };
    Ok(match value {
        AttributeValue::Bool(b) => word(0x00, &[u8::from(*b)]),
        // Sign-extended, as Solidity encodes a negative int32.
        AttributeValue::Int(n) => word(if *n < 0 { 0xFF } else { 0x00 }, &n.to_be_bytes()),
        AttributeValue::U64(n) => word(0x00, &n.to_be_bytes()),
        AttributeValue::U256(w)
        | AttributeValue::Decimal(w)
        | AttributeValue::Bytes32(w)
        | AttributeValue::EntityKey(w) => Vec::from(*w),
        AttributeValue::EthereumAddress(a) => word(0x00, a),
        AttributeValue::Str(s) => {
            if s.len() > MAX_STR_BYTES {
                return Err(AttrAbiError::StringTooLong(s.len()));
            }
            Vec::from(s.as_bytes())
        }
        AttributeValue::Bytes(b) => {
            if b.len() > MAX_PAYLOAD_BYTES {
                return Err(AttrAbiError::PayloadTooLong(b.len()));
            }
            b.clone()
        }
    })
}

/// Decode a value of type `ty` from its wire bytes.
fn decode_value(ty: AttributeType, bytes: &[u8]) -> Result<AttributeValue, AttrAbiError> {
    // The variable-width types first: everything else must be exactly one word.
    match ty {
        AttributeType::Str => {
            if bytes.len() > MAX_STR_BYTES {
                return Err(AttrAbiError::StringTooLong(bytes.len()));
            }
            return core::str::from_utf8(bytes)
                .map(|s| AttributeValue::Str(s.to_string()))
                .map_err(|_| AttrAbiError::NotUtf8);
        }
        AttributeType::Bytes => {
            if bytes.len() > MAX_PAYLOAD_BYTES {
                return Err(AttrAbiError::PayloadTooLong(bytes.len()));
            }
            return Ok(AttributeValue::Bytes(bytes.to_vec()));
        }
        _ => {}
    }

    let w: [u8; EVM_WORD_LENGTH] = bytes
        .try_into()
        .map_err(|_| AttrAbiError::BadWordLength(bytes.len()))?;
    match ty {
        AttributeType::Bool => {
            zero_prefix(&w, EVM_WORD_LENGTH - 1)?;
            match w[EVM_WORD_LENGTH - 1] {
                0 => Ok(AttributeValue::Bool(false)),
                1 => Ok(AttributeValue::Bool(true)),
                other => Err(AttrAbiError::BadBool(other)),
            }
        }
        AttributeType::Int => {
            let fill = if w[EVM_WORD_LENGTH - 4] & 0x80 == 0 {
                0x00
            } else {
                0xFF
            };
            if w[..EVM_WORD_LENGTH - 4].iter().any(|b| *b != fill) {
                return Err(AttrAbiError::BadSignExtension);
            }
            Ok(AttributeValue::Int(i32::from_be_bytes(
                w[EVM_WORD_LENGTH - 4..].try_into().unwrap(),
            )))
        }
        AttributeType::U64 => {
            zero_prefix(&w, EVM_WORD_LENGTH - 8)?;
            Ok(AttributeValue::U64(u64::from_be_bytes(
                w[EVM_WORD_LENGTH - 8..].try_into().unwrap(),
            )))
        }
        AttributeType::EthereumAddress => {
            zero_prefix(&w, EVM_WORD_LENGTH - ETH_ADDRESS_LEN)?;
            Ok(AttributeValue::EthereumAddress(
                w[EVM_WORD_LENGTH - ETH_ADDRESS_LEN..].try_into().unwrap(),
            ))
        }
        AttributeType::U256 => Ok(AttributeValue::U256(w)),
        AttributeType::Decimal => Ok(AttributeValue::Decimal(w)),
        AttributeType::Bytes32 => Ok(AttributeValue::Bytes32(w)),
        AttributeType::EntityKey => Ok(AttributeValue::EntityKey(w)),
        // Handled above.
        AttributeType::Str | AttributeType::Bytes => unreachable!("variable-width types"),
    }
}

/// Check that a right-aligned value's leading `len` bytes are zero padding.
fn zero_prefix(word: &[u8; EVM_WORD_LENGTH], len: usize) -> Result<(), AttrAbiError> {
    if word[..len].iter().any(|b| *b != 0) {
        Err(AttrAbiError::NonZeroValuePadding)
    } else {
        Ok(())
    }
}

/// Why an [`Operation`]'s `operationData` didn't survive the ABI boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpAbiError {
    /// The blob didn't ABI-decode as the struct its tag names.
    Abi(String),
    /// It decoded, but is not the canonical encoding of what it decoded to.
    NonCanonical,
}

impl fmt::Display for OpAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Abi(e) => write!(f, "invalid operationData: {e}"),
            Self::NonCanonical => write!(f, "operationData is not canonically encoded"),
        }
    }
}

impl std::error::Error for OpAbiError {}

/// Why an attribute value didn't survive the ABI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrAbiError {
    /// The `typeId` byte names no type.
    UnknownType(u8),
    /// A tombstone (`typeId` 0) carried a value; the only canonical spelling
    /// is zero-length.
    TombstoneNotEmpty(usize),
    /// A word-typed value wasn't exactly 32 bytes.
    BadWordLength(usize),
    /// A right-aligned value's leading padding wasn't zero.
    NonZeroValuePadding,
    /// A `bool` word held something other than 0 or 1.
    BadBool(u8),
    /// An `int32` wasn't sign-extended across its word.
    BadSignExtension,
    /// A `string` value wasn't valid UTF-8.
    NotUtf8,
    /// A `string` value exceeded [`MAX_STR_BYTES`].
    StringTooLong(usize),
    /// A `bytes` value exceeded [`MAX_PAYLOAD_BYTES`].
    PayloadTooLong(usize),
}

impl fmt::Display for AttrAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownType(t) => write!(f, "unknown attribute value type {t}"),
            Self::TombstoneNotEmpty(n) => {
                write!(f, "tombstone must carry no value, got {n} bytes")
            }
            Self::BadWordLength(n) => write!(f, "word-typed value must be 32 bytes, got {n}"),
            Self::NonZeroValuePadding => write!(f, "attribute value has non-zero leading padding"),
            Self::BadBool(b) => write!(f, "bool value must be 0 or 1, got {b}"),
            Self::BadSignExtension => write!(f, "int value is not sign-extended"),
            Self::NotUtf8 => write!(f, "string value is not valid UTF-8"),
            Self::StringTooLong(n) => {
                write!(f, "string value exceeds {MAX_STR_BYTES} bytes ({n})")
            }
            Self::PayloadTooLong(n) => {
                write!(f, "payload exceeds {MAX_PAYLOAD_BYTES} bytes ({n})")
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

    fn attr(n: &str, value: &AttributeValue) -> Attribute {
        Attribute::from_value(name(n), value).unwrap()
    }

    fn u256(n: &str, v: u64) -> Attribute {
        attr(n, &AttributeValue::u256_from_u64(v))
    }

    // -------------------------------------------------------------------------
    // Operation constructors
    // -------------------------------------------------------------------------

    /// Each constructor tags its op and round-trips its own payload struct.
    #[test]
    fn each_op_round_trips_its_payload() {
        let key = B256::repeat_byte(9);
        let owner = Address::repeat_byte(0xAB);

        let create = Operation::create(7, 100, 50, FLAGS_RO, vec![]);
        assert_eq!(create.operation, OP_CREATE);
        let c: Create = create.payload_of().unwrap();
        assert_eq!(
            (c.salt, c.expiresAt, c.minLifetime, c.creationFlags),
            (7, 100, 50, FLAGS_RO)
        );

        let patch = Operation::patch(key, vec![u256("a", 1)]);
        assert_eq!(patch.operation, OP_PATCH);
        let p: Patch = patch.payload_of().unwrap();
        assert_eq!((p.entityKey, p.mutations.len()), (key, 1));

        let extend = Operation::extend_expiry(key, 500, 10);
        assert_eq!(extend.operation, OP_EXTEND_EXPIRY);
        let e: ExtendExpiry = extend.payload_of().unwrap();
        assert_eq!((e.entityKey, e.expiresAt, e.minLifetime), (key, 500, 10));

        let transfer = Operation::transfer_ownership(key, owner);
        assert_eq!(transfer.operation, OP_TRANSFER_OWNERSHIP);
        let t: TransferOwnership = transfer.payload_of().unwrap();
        assert_eq!((t.entityKey, t.newOwner), (key, owner));

        let delete = Operation::delete(key);
        assert_eq!(delete.operation, OP_DELETE);
        let d: Delete = delete.payload_of().unwrap();
        assert_eq!(d.entityKey, key);
    }

    const FLAGS_RO: u8 = 1;

    /// Trailing junk decodes fine under a permissive ABI reader but is not what
    /// the encoder emits — so two blobs would mean one op. Rejected.
    #[test]
    fn payload_of_rejects_non_canonical_data() {
        let mut op = Operation::delete(B256::repeat_byte(1));
        op.operationData = {
            let mut b = op.operationData.to_vec();
            b.extend_from_slice(&[0u8; 32]);
            Bytes::from(b)
        };
        assert_eq!(op.payload_of::<Delete>(), Err(OpAbiError::NonCanonical));
    }

    #[test]
    fn payload_of_rejects_a_blob_of_the_wrong_shape() {
        let op = Operation {
            operation: OP_DELETE,
            operationData: Bytes::from_static(&[0xAB, 0xCD]),
        };
        assert!(matches!(op.payload_of::<Delete>(), Err(OpAbiError::Abi(_))));
    }

    #[test]
    fn constructors_sort_attributes() {
        let op = Operation::create(0, 1, 0, 0, vec![u256("z.attr", 1), u256("a.attr", 2)]);
        let c: Create = op.payload_of().unwrap();
        assert!(c.attributes[0].name < c.attributes[1].name);

        let op = Operation::patch(B256::ZERO, vec![u256("z", 1), u256("a", 2)]);
        let p: Patch = op.payload_of().unwrap();
        assert!(p.mutations[0].name < p.mutations[1].name);
    }

    // -------------------------------------------------------------------------
    // Attribute values
    // -------------------------------------------------------------------------

    /// Every type survives the round trip through its wire bytes.
    #[test]
    fn values_round_trip_through_the_wire() {
        let values = [
            AttributeValue::Bool(true),
            AttributeValue::Bool(false),
            AttributeValue::Int(-42),
            AttributeValue::Int(i32::MIN),
            AttributeValue::Int(i32::MAX),
            AttributeValue::U64(0),
            AttributeValue::U64(u64::MAX),
            AttributeValue::u256_from_u64(42),
            AttributeValue::Decimal([0xFF; 32]),
            AttributeValue::Bytes32([0x5A; 32]),
            AttributeValue::Str("hello".into()),
            AttributeValue::Str("é".repeat(64)), // 128 bytes, the maximum
            AttributeValue::Bytes(vec![0xEE; 4096]),
            AttributeValue::EthereumAddress([0x11; 20]),
            AttributeValue::EntityKey([0x77; 32]),
        ];
        for value in values {
            let a = Attribute::from_value(name("a"), &value).unwrap();
            assert_eq!(a.typeId, value.type_id());
            assert_eq!(a.to_value().unwrap(), Some(value.clone()), "{value:?}");
        }
    }

    /// Word types occupy exactly one word; the variable ones are unpadded.
    #[test]
    fn wire_widths_match_the_type() {
        assert_eq!(
            attr("a", &AttributeValue::u256_from_u64(42)).value.len(),
            32
        );
        assert_eq!(attr("a", &AttributeValue::Bool(true)).value.len(), 32);
        assert_eq!(attr("a", &AttributeValue::U64(1)).value.len(), 32);
        // Raw, not padded to a word.
        assert_eq!(
            attr("a", &AttributeValue::Str("hi".into())).value.as_ref(),
            b"hi"
        );
        assert_eq!(
            attr("a", &AttributeValue::Bytes(vec![1, 2, 3])).value.len(),
            3
        );
    }

    #[test]
    fn u64_encodes_right_aligned() {
        let a = attr("a", &AttributeValue::U64(42));
        assert_eq!(a.value[31], 42);
        assert!(a.value[..31].iter().all(|b| *b == 0));
    }

    // -------------------------------------------------------------------------
    // Tombstones
    // -------------------------------------------------------------------------

    #[test]
    fn tombstone_is_type_zero_with_no_value() {
        let t = Attribute::tombstone(name("gone"));
        assert!(t.is_tombstone());
        assert_eq!(t.typeId, TOMBSTONE_TYPE_ID);
        assert!(t.value.is_empty());
        assert_eq!(t.to_value(), Ok(None));
    }

    /// One spelling only — a tombstone carrying bytes is a revert, not a value
    /// to ignore, so it can't be used to smuggle a second encoding of "unset".
    #[test]
    fn tombstone_with_a_value_is_rejected() {
        let bad = Attribute {
            name: name("x").0,
            typeId: TOMBSTONE_TYPE_ID,
            value: Bytes::from_static(&[0u8; 32]),
        };
        assert_eq!(bad.to_value(), Err(AttrAbiError::TombstoneNotEmpty(32)));
    }

    /// A set value never reads back as a tombstone, whatever its bytes.
    #[test]
    fn a_zero_value_is_not_a_tombstone() {
        let zero = attr("a", &AttributeValue::u256_from_u64(0));
        assert!(!zero.is_tombstone());
        assert_eq!(
            zero.to_value().unwrap(),
            Some(AttributeValue::u256_from_u64(0))
        );
    }

    // -------------------------------------------------------------------------
    // Malformed wire bytes
    // -------------------------------------------------------------------------

    #[test]
    fn rejects_oversized_values() {
        let long = AttributeValue::Str("x".repeat(MAX_STR_BYTES + 1));
        assert_eq!(
            Attribute::from_value(name("big"), &long),
            Err(AttrAbiError::StringTooLong(MAX_STR_BYTES + 1))
        );
        assert!(
            Attribute::from_value(
                name("full"),
                &AttributeValue::Str("x".repeat(MAX_STR_BYTES))
            )
            .is_ok()
        );
        let huge = AttributeValue::Bytes(vec![0; MAX_PAYLOAD_BYTES + 1]);
        assert_eq!(
            Attribute::from_value(name("p"), &huge),
            Err(AttrAbiError::PayloadTooLong(MAX_PAYLOAD_BYTES + 1))
        );
    }

    /// Malformed bytes a hand-rolled encoder could produce are rejected, not
    /// silently truncated.
    #[test]
    fn to_value_rejects_malformed_bytes() {
        let with = |ty: u8, bytes: Vec<u8>| Attribute {
            name: name("a").0,
            typeId: ty,
            value: Bytes::from(bytes),
        };
        let word = |tail: &[u8]| {
            let mut w = vec![0u8; EVM_WORD_LENGTH];
            w[EVM_WORD_LENGTH - tail.len()..].copy_from_slice(tail);
            w
        };

        assert_eq!(
            with(AttributeType::Bool.id(), word(&[2])).to_value(),
            Err(AttrAbiError::BadBool(2))
        );

        // An int32 whose upper bytes don't match its sign bit.
        let mut bad_int = word(&[1]);
        bad_int[0] = 0xFF;
        assert_eq!(
            with(AttributeType::Int.id(), bad_int).to_value(),
            Err(AttrAbiError::BadSignExtension)
        );

        // An address with junk in its leading padding.
        assert_eq!(
            with(
                AttributeType::EthereumAddress.id(),
                vec![0xAB; EVM_WORD_LENGTH]
            )
            .to_value(),
            Err(AttrAbiError::NonZeroValuePadding)
        );

        // A u64 with junk above its eight bytes.
        assert_eq!(
            with(AttributeType::U64.id(), vec![0xAB; EVM_WORD_LENGTH]).to_value(),
            Err(AttrAbiError::NonZeroValuePadding)
        );

        // A word type that isn't a word wide.
        assert_eq!(
            with(AttributeType::U256.id(), vec![1, 2, 3]).to_value(),
            Err(AttrAbiError::BadWordLength(3))
        );

        assert_eq!(
            with(AttributeType::Str.id(), vec![0xFF, 0xFE]).to_value(),
            Err(AttrAbiError::NotUtf8)
        );

        assert_eq!(
            with(99, vec![]).to_value(),
            Err(AttrAbiError::UnknownType(99))
        );
    }

    #[test]
    fn sort_orders_by_name_ascending() {
        let mut attrs = vec![u256("c", 1), u256("a", 2), u256("b", 3)];
        Attribute::sort(&mut attrs);
        let names: Vec<_> = attrs.iter().map(|a| a.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }
}
