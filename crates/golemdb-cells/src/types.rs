//! The type-id space: [`CellType`] and the pieces it is built from.

use crate::MAX_VALUE_LEN;
use crate::error::CellParseError;
#[cfg(feature = "custom_types")]
use crate::{CUSTOM_TYPE_ID_BASE, TYPE_ID_SPACE};

/// The width exponent `w` of a `4 · 2^w`-byte family: 4, 8, 16 or 32 bytes.
///
/// The four members of such a family sit at consecutive type ids `base + w`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Width {
    W4,
    W8,
    W16,
    W32,
}

impl Width {
    /// The `w` in `4 · 2^w`, i.e. the type id's offset within its family.
    pub const fn w(self) -> u8 {
        match self {
            Self::W4 => 0,
            Self::W8 => 1,
            Self::W16 => 2,
            Self::W32 => 3,
        }
    }

    /// Inverse of [`Width::w`]; `w` must be 0–3.
    const fn from_w(w: u8) -> Self {
        match w {
            0 => Self::W4,
            1 => Self::W8,
            2 => Self::W16,
            _ => Self::W32,
        }
    }

    /// The value width in bytes: `4 · 2^w`.
    pub const fn bytes(self) -> usize {
        4usize << self.w()
    }
}

/// The width of a float: `f32` or `f64`.
///
/// `f16` and `f128` are reserved at ids 26–27, so this is deliberately not a
/// `4 · 2^w` family — it is a two-member family at ids 24–25.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FloatWidth {
    F32,
    F64,
}

impl FloatWidth {
    pub const fn bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

/// A custom, per-deployment type id from the 64–127 block.
///
/// A newtype so a `CellType::Custom` cannot be built holding an id from the core
/// block. What the id *means* is up to the deployment's type registry, so this
/// layer only frames a custom value — [`ValueLayout::LengthPrefixed`] — and
/// never inspects its content.
///
/// Behind the `custom_types` feature, which is off by default.
#[cfg(feature = "custom_types")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CustomTypeId(u8);

#[cfg(feature = "custom_types")]
impl CustomTypeId {
    /// `id` must be in 64–127; anything else is not a custom type.
    pub const fn new(id: u8) -> Option<Self> {
        if id >= CUSTOM_TYPE_ID_BASE && id < TYPE_ID_SPACE {
            Some(Self(id))
        } else {
            None
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// How a type's value bytes are framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueLayout {
    /// Exactly this many bytes, no length byte — the width is in the type id.
    Fixed(usize),
    /// A length byte, then that many bytes, up to `max`.
    LengthPrefixed { max: usize },
}

impl ValueLayout {
    /// Whether a cell of this layout carries a length byte after its metadata.
    pub const fn has_length_byte(self) -> bool {
        matches!(self, Self::LengthPrefixed { .. })
    }
}

/// The type of a cell's value — the 7-bit type-id space, decoded.
///
/// | id     | type                                  | family                | value bytes               | order-encoding     |
/// | ------ | ------------------------------------- | --------------------- | ------------------------- | ------------------ |
/// | 0      | tombstone                             | singleton             | 0                         | —                  |
/// | 1      | `bool`                                | singleton             | 1                         | —                  |
/// | 2      | `str`                                 | singleton             | var (≤ [`MAX_VALUE_LEN`]) | raw UTF-8          |
/// | 3      | `bytes` (field-only)                  | singleton             | var (≤ [`MAX_VALUE_LEN`]) | —                  |
/// | 4      | `bytes20`                             | singleton             | 20                        | plain bytes        |
/// | 5–7    | *reserved singletons*                 |                       |                           |                    |
/// | 8–11   | `bytes4` `bytes8` `bytes16` `bytes32` | `8 + w`               | 4·2^w                     | plain bytes        |
/// | 12–15  | `u32` `u64` `u128` `u256`             | `12 + w`              | 4·2^w                     | plain BE           |
/// | 16–19  | `i32` `i64` `i128` `i256`             | `16 + w`              | 4·2^w                     | sign-bit-biased BE |
/// | 20–23  | `dec32` `dec64` `dec128` `dec256`     | `20 + w`              | 4·2^w (fixed scale)       | sign-bit-biased BE |
/// | 24–25  | `f32` `f64`                           | floats (26–27 rsvd)   | 4 / 8                     | IEEE total-order   |
/// | 28–29  | `date32` `timestamp64`                | time (30–31 rsvd)     | 4 / 8                     | sign-bit-biased BE |
/// | 32–63  | *reserved — future core families*     | 8 aligned blocks of 4 |                           |                    |
/// | 64–127 | *custom types* (`custom_types`)       | per deployment        | var (≤ [`MAX_VALUE_LEN`]) |                    |
///
/// The order-encoding column says how a value must be laid out for a bytewise
/// comparison to match a value comparison. It is recorded here but not applied
/// here: `AttributeValue::index_bytes` in `arkiv-interfaces` owns those
/// transforms today. Whether an ordered type is actually *offered* for range
/// queries is a separate, policy question — `QueryCapabilities` answers it, and
/// answers "no" for some types this column can order (`bytes32`, for one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    /// A deleted cell: no value bytes at all.
    Tombstone,
    Bool,
    Str,
    /// Field-only: storable, but never range-indexed. Variable width, and byte
    /// blobs have no meaningful order.
    Bytes,
    /// An address-width byte string; its own singleton rather than part of the
    /// `4 · 2^w` family below, because 20 is not a power-of-two multiple of 4.
    Bytes20,
    /// `bytes4`, `bytes8`, `bytes16`, `bytes32`.
    FixedBytes(Width),
    /// `u32`, `u64`, `u128`, `u256`.
    Uint(Width),
    /// `i32`, `i64`, `i128`, `i256`.
    Int(Width),
    /// `dec32`, `dec64`, `dec128`, `dec256`, each at the fixed scale the spec
    /// pins for its width.
    Decimal(Width),
    /// `f32`, `f64`.
    Float(FloatWidth),
    /// Days since the Unix epoch, signed.
    Date32,
    /// Microseconds since the Unix epoch, signed.
    Timestamp64,
    /// A per-deployment type from the 64–127 block. Behind the `custom_types`
    /// feature; without it those ids are rejected as
    /// [`CellParseError::CustomTypesDisabled`].
    #[cfg(feature = "custom_types")]
    Custom(CustomTypeId),
}

impl CellType {
    /// The type id this type occupies in the metadata byte's low 7 bits.
    pub const fn id(self) -> u8 {
        match self {
            Self::Tombstone => 0,
            Self::Bool => 1,
            Self::Str => 2,
            Self::Bytes => 3,
            Self::Bytes20 => 4,
            Self::FixedBytes(w) => 8 + w.w(),
            Self::Uint(w) => 12 + w.w(),
            Self::Int(w) => 16 + w.w(),
            Self::Decimal(w) => 20 + w.w(),
            Self::Float(FloatWidth::F32) => 24,
            Self::Float(FloatWidth::F64) => 25,
            Self::Date32 => 28,
            Self::Timestamp64 => 29,
            #[cfg(feature = "custom_types")]
            Self::Custom(c) => c.get(),
        }
    }

    /// The type's name as the spec's table writes it — `"bytes20"`, `"u64"`,
    /// `"dec128"`. What errors and diagnostics print.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tombstone => "tombstone",
            Self::Bool => "bool",
            Self::Str => "str",
            Self::Bytes => "bytes",
            Self::Bytes20 => "bytes20",
            Self::FixedBytes(w) => match w {
                Width::W4 => "bytes4",
                Width::W8 => "bytes8",
                Width::W16 => "bytes16",
                Width::W32 => "bytes32",
            },
            Self::Uint(w) => match w {
                Width::W4 => "u32",
                Width::W8 => "u64",
                Width::W16 => "u128",
                Width::W32 => "u256",
            },
            Self::Int(w) => match w {
                Width::W4 => "i32",
                Width::W8 => "i64",
                Width::W16 => "i128",
                Width::W32 => "i256",
            },
            Self::Decimal(w) => match w {
                Width::W4 => "dec32",
                Width::W8 => "dec64",
                Width::W16 => "dec128",
                Width::W32 => "dec256",
            },
            Self::Float(FloatWidth::F32) => "f32",
            Self::Float(FloatWidth::F64) => "f64",
            Self::Date32 => "date32",
            Self::Timestamp64 => "timestamp64",
            #[cfg(feature = "custom_types")]
            Self::Custom(_) => "custom",
        }
    }

    /// Decode a type id. `id` must already be masked to 7 bits; ids the spec
    /// reserves come back as [`CellParseError::ReservedType`].
    pub const fn from_id(id: u8) -> Result<Self, CellParseError> {
        match id {
            0 => Ok(Self::Tombstone),
            1 => Ok(Self::Bool),
            2 => Ok(Self::Str),
            3 => Ok(Self::Bytes),
            4 => Ok(Self::Bytes20),
            8..=11 => Ok(Self::FixedBytes(Width::from_w(id - 8))),
            12..=15 => Ok(Self::Uint(Width::from_w(id - 12))),
            16..=19 => Ok(Self::Int(Width::from_w(id - 16))),
            20..=23 => Ok(Self::Decimal(Width::from_w(id - 20))),
            24 => Ok(Self::Float(FloatWidth::F32)),
            25 => Ok(Self::Float(FloatWidth::F64)),
            28 => Ok(Self::Date32),
            29 => Ok(Self::Timestamp64),
            #[cfg(feature = "custom_types")]
            64..=127 => Ok(Self::Custom(CustomTypeId(id))),
            #[cfg(not(feature = "custom_types"))]
            64..=127 => Err(CellParseError::CustomTypesDisabled(id)),
            // 5–7, 26–27, 30–31 and 32–63 are reserved; 128.. cannot fit the
            // 7-bit field and is treated the same way.
            _ => Err(CellParseError::ReservedType(id)),
        }
    }

    /// How this type's value bytes are framed.
    ///
    /// Custom types are length-prefixed because this layer cannot know their
    /// widths, and a cell whose length only the deployment's registry knows
    /// would not be self-delimiting.
    pub const fn layout(self) -> ValueLayout {
        match self {
            Self::Tombstone => ValueLayout::Fixed(0),
            Self::Bool => ValueLayout::Fixed(1),
            #[cfg(feature = "custom_types")]
            Self::Custom(_) => ValueLayout::LengthPrefixed { max: MAX_VALUE_LEN },
            Self::Str | Self::Bytes => ValueLayout::LengthPrefixed { max: MAX_VALUE_LEN },
            Self::Bytes20 => ValueLayout::Fixed(20),
            Self::FixedBytes(w) | Self::Uint(w) | Self::Int(w) | Self::Decimal(w) => {
                ValueLayout::Fixed(w.bytes())
            }
            Self::Float(f) => ValueLayout::Fixed(f.bytes()),
            Self::Date32 => ValueLayout::Fixed(4),
            Self::Timestamp64 => ValueLayout::Fixed(8),
        }
    }

    /// Check that `value` is a well-formed body for this type.
    pub fn validate(self, value: &[u8]) -> Result<(), CellParseError> {
        match self.layout() {
            ValueLayout::Fixed(n) if value.len() != n => {
                return Err(CellParseError::LengthMismatch {
                    ty: self,
                    expected: n,
                    actual: value.len(),
                });
            }
            ValueLayout::LengthPrefixed { max } if value.len() > max => {
                return Err(CellParseError::TooLong {
                    max,
                    actual: value.len(),
                });
            }
            _ => {}
        }
        match (self, value) {
            (Self::Bool, [b]) if *b > 1 => Err(CellParseError::InvalidBool(*b)),
            (Self::Str, v) => match core::str::from_utf8(v) {
                Ok(_) => Ok(()),
                // `valid_up_to` is where the decoder stopped, so the snippet
                // starts on the offending byte rather than the value's start.
                Err(e) => Err(CellParseError::invalid_utf8(v, e.valid_up_to())),
            },
            _ => Ok(()),
        }
    }
}
