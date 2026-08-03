//! The entity data types passed across the traits.
//!
//! Plain data, no encoding: turning an [`Entity`] into the bytes an
//! [`EntityStore`](crate::state::EntityStore) holds (and back) is the host's job.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::primitives::{Address, BlockNumber, EntityKey};

/// A stored entity: a fixed identity, a changing lifecycle, opaque contents, and
/// queryable attributes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entity {
    /// Unique key. Set at creation, never changes.
    pub key: EntityKey,
    /// Who created it. Set at creation, never changes.
    pub creator: Address,
    /// Who owns it now. Changes on transfer.
    pub owner: Address,
    /// The block it was created in.
    pub created_at_block: BlockNumber,
    /// The block of its last change (create, update, extend, or transfer).
    pub last_modified_at_block: BlockNumber,
    /// The block it expires in.
    pub expires_at: BlockNumber,
    /// Opaque content type (e.g. a MIME string).
    pub content_type: Vec<u8>,
    /// Opaque application payload.
    pub payload: Vec<u8>,
    /// Queryable attributes.
    pub attributes: Vec<Attribute>,
}

/// An [`Entity`] without its payload or attributes — enough for a query that only
/// needs identity and lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntityMeta {
    pub key: EntityKey,
    pub creator: Address,
    pub owner: Address,
    pub created_at_block: BlockNumber,
    pub last_modified_at_block: BlockNumber,
    pub expires_at: BlockNumber,
    pub content_type: Vec<u8>,
}

/// A `(key, value)` attribute. The value carries its own type — see
/// [`AttributeValue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub key: Vec<u8>,
    pub value: AttributeValue,
}

impl Attribute {
    /// A named attribute with `value`.
    pub fn new(key: impl Into<Vec<u8>>, value: AttributeValue) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }

    /// The value's [`AttributeType`] — what its `typeId` tag says.
    pub const fn value_type(&self) -> AttributeType {
        self.value.attr_type()
    }
}

/// A typed attribute value.
///
/// The type is part of the value, so a stored attribute is never a bag of bytes
/// needing an out-of-band tag to read. Each variant maps to one `typeId`
/// ([`AttributeType`]), one ABI/Solidity wire type, and one indexing behaviour:
///
/// | typeId | variant                | ABI type  | indexing            |
/// |--------|------------------------|-----------|---------------------|
/// | 1      | [`Bool`](Self::Bool)   | `bool`    | equality            |
/// | 2      | [`Int`](Self::Int)     | `int32`   | equality + range    |
/// | 3      | [`U256`](Self::U256)   | `uint256` | equality + range    |
/// | 4      | [`Decimal`](Self::Decimal) | `int256` | equality + range |
/// | 5      | [`Bytes32`](Self::Bytes32) | `bytes32` | equality        |
/// | 6      | [`Bytes`](Self::Bytes) | `bytes`   | none — system-only  |
/// | 7      | [`Str`](Self::Str)     | `string`  | equality + prefix   |
/// | 8      | [`EthereumAddress`](Self::EthereumAddress) | `address` | equality |
/// | 9      | [`EntityKey`](Self::EntityKey) | `bytes32` | equality    |
///
/// Two byte encodings hang off this type, and they are **not** the same thing:
/// [`encode`](Self::encode) is the canonical *storage* form (natural width, no
/// tricks), while [`index_bytes`](Self::index_bytes) is the *index* form, chosen so
/// that byte order equals numeric order for the range-indexable types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeValue {
    /// A boolean.
    Bool(bool),
    /// A 32-bit signed integer — the default signed int.
    Int(i32),
    /// A 256-bit unsigned integer, big-endian.
    U256([u8; 32]),
    /// A fixed-point decimal: a 256-bit **signed** integer (two's complement,
    /// big-endian) scaled by [`DECIMAL_SCALE`] decimal places, so `1.5` is
    /// `1_500_000_000_000_000_000`.
    Decimal([u8; 32]),
    /// 32 opaque bytes.
    Bytes32([u8; 32]),
    /// Opaque variable-length bytes. **System-only** (`$payload`): it never appears
    /// in an operation's attribute array and is never indexed.
    Bytes(Vec<u8>),
    /// A UTF-8 string.
    Str(String),
    /// A 20-byte Ethereum address.
    EthereumAddress(Address),
    /// A reference to another entity. Weak by spec: no existence check at write
    /// time, and dangling references are permitted.
    EntityKey(EntityKey),
}

/// Decimal places a [`Decimal`](AttributeValue::Decimal) is scaled by — fixed, so
/// two decimals are always directly comparable (and range-indexable) as integers.
pub const DECIMAL_SCALE: u32 = 18;

impl AttributeValue {
    /// This value's type tag.
    pub const fn attr_type(&self) -> AttributeType {
        match self {
            Self::Bool(_) => AttributeType::Bool,
            Self::Int(_) => AttributeType::Int,
            Self::U256(_) => AttributeType::U256,
            Self::Decimal(_) => AttributeType::Decimal,
            Self::Bytes32(_) => AttributeType::Bytes32,
            Self::Bytes(_) => AttributeType::Bytes,
            Self::Str(_) => AttributeType::Str,
            Self::EthereumAddress(_) => AttributeType::EthereumAddress,
            Self::EntityKey(_) => AttributeType::EntityKey,
        }
    }

    /// This value's `typeId` — the byte the wire and the storage record carry.
    pub const fn type_id(&self) -> u8 {
        self.attr_type().id()
    }

    /// A `u64` as a [`U256`](Self::U256) — right-aligned big-endian, so it sorts
    /// numerically.
    pub fn u256_from_u64(n: u64) -> Self {
        let mut buf = [0u8; 32];
        buf[32 - size_of::<u64>()..].copy_from_slice(&n.to_be_bytes());
        Self::U256(buf)
    }

    /// The canonical **storage** bytes: the value at its natural width, big-endian,
    /// with no tag (the tag travels alongside as a [`type_id`](Self::type_id)).
    /// [`decode`](Self::decode) is the exact inverse.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Bool(b) => Vec::from([u8::from(*b)]),
            Self::Int(n) => Vec::from(n.to_be_bytes()),
            Self::U256(w) | Self::Decimal(w) | Self::Bytes32(w) | Self::EntityKey(w) => {
                Vec::from(*w)
            }
            Self::Bytes(b) => b.clone(),
            Self::Str(s) => Vec::from(s.as_bytes()),
            Self::EthereumAddress(a) => Vec::from(*a),
        }
    }

    /// Read a value of type `ty` back from its [`encode`](Self::encode) bytes.
    pub fn decode(ty: AttributeType, bytes: &[u8]) -> Result<Self, AttributeValueError> {
        let fixed = |want: usize| -> Result<(), AttributeValueError> {
            if bytes.len() == want {
                Ok(())
            } else {
                Err(AttributeValueError::BadLength {
                    ty,
                    expected: want,
                    got: bytes.len(),
                })
            }
        };
        let word = || -> Result<[u8; 32], AttributeValueError> {
            fixed(32)?;
            let mut w = [0u8; 32];
            w.copy_from_slice(bytes);
            Ok(w)
        };
        match ty {
            AttributeType::Bool => {
                fixed(1)?;
                match bytes[0] {
                    0 => Ok(Self::Bool(false)),
                    1 => Ok(Self::Bool(true)),
                    other => Err(AttributeValueError::BadBool(other)),
                }
            }
            AttributeType::Int => {
                fixed(4)?;
                let mut n = [0u8; 4];
                n.copy_from_slice(bytes);
                Ok(Self::Int(i32::from_be_bytes(n)))
            }
            AttributeType::U256 => Ok(Self::U256(word()?)),
            AttributeType::Decimal => Ok(Self::Decimal(word()?)),
            AttributeType::Bytes32 => Ok(Self::Bytes32(word()?)),
            AttributeType::EntityKey => Ok(Self::EntityKey(word()?)),
            AttributeType::Bytes => Ok(Self::Bytes(bytes.to_vec())),
            AttributeType::Str => match core::str::from_utf8(bytes) {
                Ok(s) => Ok(Self::Str(String::from(s))),
                Err(_) => Err(AttributeValueError::NotUtf8),
            },
            AttributeType::EthereumAddress => {
                fixed(20)?;
                let mut a = [0u8; 20];
                a.copy_from_slice(bytes);
                Ok(Self::EthereumAddress(a))
            }
        }
    }

    /// The **index** bytes: the value encoded so that byte order equals value
    /// order within its type.
    ///
    /// Same as [`encode`](Self::encode) except for the signed types
    /// ([`Int`](Self::Int), [`Decimal`](Self::Decimal)), whose two's-complement
    /// bytes sort negatives *above* positives. Biasing by half the range — a flip
    /// of the sign bit — restores numeric order, which is what makes a range scan
    /// over a plain byte-ordered structure correct.
    ///
    /// These bytes are what the index is keyed on, so this encoding is
    /// consensus-critical.
    pub fn index_bytes(&self) -> Vec<u8> {
        match self {
            Self::Int(n) => Vec::from((*n as u32 ^ SIGN_BIT_32).to_be_bytes()),
            Self::Decimal(w) => {
                let mut biased = *w;
                biased[0] ^= 0x80;
                Vec::from(biased)
            }
            other => other.encode(),
        }
    }
}

/// The sign bit of a 32-bit two's-complement integer — flipped to bias an
/// [`Int`](AttributeValue::Int) into numeric byte order.
const SIGN_BIT_32: u32 = 1 << 31;

/// An attribute value's `typeId` — the tag that travels with the value on the wire
/// and in storage.
///
/// The discriminants are the protocol's `typeId`s and are **consensus-critical**:
/// they are written into entity records and mixed into index keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AttributeType {
    Bool = 1,
    Int = 2,
    U256 = 3,
    Decimal = 4,
    Bytes32 = 5,
    Bytes = 6,
    Str = 7,
    EthereumAddress = 8,
    EntityKey = 9,
}

impl AttributeType {
    /// The `typeId` byte.
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// The type a `typeId` names, or `None` if no type has that tag.
    pub const fn from_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(Self::Bool),
            2 => Some(Self::Int),
            3 => Some(Self::U256),
            4 => Some(Self::Decimal),
            5 => Some(Self::Bytes32),
            6 => Some(Self::Bytes),
            7 => Some(Self::Str),
            8 => Some(Self::EthereumAddress),
            9 => Some(Self::EntityKey),
            _ => None,
        }
    }

    /// Whether a client may set an attribute of this type.
    ///
    /// [`Bytes`](Self::Bytes) is system-only — it backs `$payload`, which travels
    /// in its own operation field, never in the attribute array.
    pub const fn is_user_settable(self) -> bool {
        !matches!(self, Self::Bytes)
    }

    /// The spec's name for this type — the same spelling the query language's
    /// type tags use (`i32(10)`, `str('Bob')`) and the wire reports in an
    /// attribute's `type` field, so one vocabulary covers queries, responses and
    /// error messages.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "i32",
            Self::U256 => "u256",
            Self::Decimal => "dec",
            Self::Bytes32 => "bytes32",
            Self::Bytes => "bytes",
            Self::Str => "str",
            Self::EthereumAddress => "addr",
            Self::EntityKey => "key",
        }
    }
}

/// Why reading an [`AttributeValue`] back from its stored bytes failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeValueError {
    /// A fixed-width type's bytes were the wrong length.
    BadLength {
        ty: AttributeType,
        expected: usize,
        got: usize,
    },
    /// A `bool` byte was neither `0` nor `1`.
    BadBool(u8),
    /// A `string` value wasn't valid UTF-8.
    NotUtf8,
}

impl fmt::Display for AttributeValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLength { ty, expected, got } => {
                write!(f, "{} value must be {expected} bytes, got {got}", ty.name())
            }
            Self::BadBool(b) => write!(f, "bool value must be 0 or 1, got {b}"),
            Self::NotUtf8 => write!(f, "string value is not valid UTF-8"),
        }
    }
}

/// The attribute names the protocol sets on every entity.
///
/// Some never change (`$creator`, `$createdAtBlock`, `$key`); `$owner` changes on
/// transfer, `$expiration` on extend, `$contentType` on update. `$all` matches
/// every live entity.
pub mod annotations {
    /// Matches every live entity.
    pub const ALL: &[u8] = b"$all";
    /// The creator's address (never changes).
    pub const CREATOR: &[u8] = b"$creator";
    /// The creation block, as a big-endian `u64` (never changes).
    pub const CREATED_AT_BLOCK: &[u8] = b"$createdAtBlock";
    /// The current owner (changes on transfer).
    pub const OWNER: &[u8] = b"$owner";
    /// The full entity key, 32 raw bytes (never changes).
    pub const KEY: &[u8] = b"$key";
    /// The expiry block, as a big-endian `u64` (changes on extend).
    pub const EXPIRATION: &[u8] = b"$expiration";
    /// The content type (changes on update).
    pub const CONTENT_TYPE: &[u8] = b"$contentType";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every type's `typeId` is pinned by the spec table.
    #[test]
    fn type_ids_match_the_spec() {
        let table = [
            (AttributeType::Bool, 1u8),
            (AttributeType::Int, 2),
            (AttributeType::U256, 3),
            (AttributeType::Decimal, 4),
            (AttributeType::Bytes32, 5),
            (AttributeType::Bytes, 6),
            (AttributeType::Str, 7),
            (AttributeType::EthereumAddress, 8),
            (AttributeType::EntityKey, 9),
        ];
        for (ty, id) in table {
            assert_eq!(ty.id(), id);
            assert_eq!(AttributeType::from_id(id), Some(ty));
        }
        assert_eq!(AttributeType::from_id(0), None);
        assert_eq!(AttributeType::from_id(10), None);
    }

    fn samples() -> Vec<AttributeValue> {
        Vec::from([
            AttributeValue::Bool(true),
            AttributeValue::Bool(false),
            AttributeValue::Int(-7),
            AttributeValue::Int(i32::MAX),
            AttributeValue::U256([0xAB; 32]),
            AttributeValue::Decimal([0xCD; 32]),
            AttributeValue::Bytes32([0x01; 32]),
            AttributeValue::Bytes(Vec::from(b"raw".as_slice())),
            AttributeValue::Str(String::from("héllo")),
            AttributeValue::EthereumAddress([0x11; 20]),
            AttributeValue::EntityKey([0x22; 32]),
        ])
    }

    #[test]
    fn storage_encoding_round_trips_every_type() {
        for value in samples() {
            let bytes = value.encode();
            let back = AttributeValue::decode(value.attr_type(), &bytes).unwrap();
            assert_eq!(back, value);
        }
    }

    #[test]
    fn decode_rejects_malformed_values() {
        assert!(matches!(
            AttributeValue::decode(AttributeType::Bool, &[2]),
            Err(AttributeValueError::BadBool(2))
        ));
        assert!(matches!(
            AttributeValue::decode(AttributeType::Int, &[0, 1]),
            Err(AttributeValueError::BadLength {
                expected: 4,
                got: 2,
                ..
            })
        ));
        assert!(matches!(
            AttributeValue::decode(AttributeType::EthereumAddress, &[0u8; 32]),
            Err(AttributeValueError::BadLength { expected: 20, .. })
        ));
        assert!(matches!(
            AttributeValue::decode(AttributeType::Str, &[0xFF, 0xFE]),
            Err(AttributeValueError::NotUtf8)
        ));
    }

    /// The whole point of the biased index encoding: sorting the index bytes sorts
    /// the values, negatives included.
    #[test]
    fn index_bytes_sort_signed_types_numerically() {
        let mut ints = [i32::MIN, -2, -1, 0, 1, i32::MAX];
        let mut encoded: Vec<Vec<u8>> = ints
            .iter()
            .map(|n| AttributeValue::Int(*n).index_bytes())
            .collect();
        encoded.sort();
        ints.sort();
        let expected: Vec<Vec<u8>> = ints
            .iter()
            .map(|n| AttributeValue::Int(*n).index_bytes())
            .collect();
        assert_eq!(encoded, expected);
        // -1 (all ones) must sort below 0.
        assert!(AttributeValue::Int(-1).index_bytes() < AttributeValue::Int(0).index_bytes());
        // A negative decimal (two's complement 0xFF..) sorts below a positive one.
        let minus_one = AttributeValue::Decimal([0xFF; 32]);
        let plus_one = {
            let mut w = [0u8; 32];
            w[31] = 1;
            AttributeValue::Decimal(w)
        };
        assert!(minus_one.index_bytes() < plus_one.index_bytes());
    }

    /// Unsigned and unordered types index as their plain storage bytes.
    #[test]
    fn index_bytes_are_storage_bytes_for_unsigned_types() {
        for value in samples() {
            match value {
                AttributeValue::Int(_) | AttributeValue::Decimal(_) => {}
                other => assert_eq!(other.index_bytes(), other.encode()),
            }
        }
    }

    #[test]
    fn u256_from_u64_is_right_aligned() {
        let AttributeValue::U256(w) = AttributeValue::u256_from_u64(60) else {
            panic!("expected u256");
        };
        assert_eq!(w[31], 60);
        assert!(w[..31].iter().all(|b| *b == 0));
    }

    #[test]
    fn only_bytes_is_system_only() {
        for id in 1..=9u8 {
            let ty = AttributeType::from_id(id).unwrap();
            assert_eq!(ty.is_user_settable(), ty != AttributeType::Bytes);
        }
    }
}
