//! Rust definitions for "Cells" in GolemDB, alongwith the logic for encoding and decoding them
//!
//! # Wire format
//!
//! A cell is one metadata byte, a length byte for the variable-width types
//! only, then the value bytes:
//!
//! ```text
//! [ indexable: 1 bit | type id: 7 bits ] [ len ]? [ value bytes … ]
//! ```
//!
//! The type id says which of the two shapes a cell has. A fixed-width type
//! carries its width in its id, so no length byte; `str`, `bytes` and the
//! custom types carry one.
//!
//! That makes every cell **self-delimiting**: its length is knowable from its
//! own bytes. Cells can be packed adjacently and walked with
//! [`Cell::parse_prefix`], and a cell truncated in transit is rejected rather
//! than read as a shorter valid value.

use core::fmt;

/// The longest variable-width value a cell may carry — what one length byte can
/// express.
///
/// This layer imposes only the format's own bound; a deployment is free to
/// enforce something tighter on top.
pub const MAX_VALUE_LEN: usize = u8::MAX as usize;

/// The metadata byte's high bit: whether the cell is indexable.
const INDEXABLE_BIT: u8 = 0b1000_0000;

/// The metadata byte's low 7 bits: the type id.
const TYPE_ID_MASK: u8 = !INDEXABLE_BIT;

/// One past the last type id: the id space is 7 bits wide.
pub const TYPE_ID_SPACE: u8 = 128;

/// The first type id belonging to the custom block.
pub const CUSTOM_TYPE_ID_BASE: u8 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellParseError {
    /// The cell is too short to hold its metadata byte.
    Empty,
    /// The type id names a slot the spec reserves for future use.
    ReservedType(u8),
    /// A variable-width cell that stops before its length byte.
    MissingLength,
    /// A fixed-width type whose value is not exactly that wide.
    LengthMismatch { expected: usize, actual: usize },
    /// The length byte declares more value bytes than the cell carries — the
    /// cell was cut short.
    Truncated { declared: usize, actual: usize },
    /// Bytes left over after the cell this slice declares. Use
    /// [`Cell::parse_prefix`] to walk a run of packed cells.
    TrailingBytes { extra: usize },
    /// A value longer than one length byte can express.
    TooLong { max: usize, actual: usize },
    /// A `bool` whose byte is neither 0 nor 1.
    InvalidBool(u8),
    /// A `str` whose bytes are not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for CellParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "cell is missing its metadata byte"),
            Self::ReservedType(id) => write!(f, "type id {id} is reserved"),
            Self::MissingLength => write!(f, "cell is missing its length byte"),
            Self::LengthMismatch { expected, actual } => {
                write!(f, "expected {expected} value bytes, got {actual}")
            }
            Self::Truncated { declared, actual } => {
                write!(
                    f,
                    "cell declares {declared} value bytes but carries {actual}"
                )
            }
            Self::TrailingBytes { extra } => {
                write!(f, "{extra} bytes left over after the cell")
            }
            Self::TooLong { max, actual } => {
                write!(f, "value is {actual} bytes, the maximum is {max}")
            }
            Self::InvalidBool(b) => write!(f, "bool byte must be 0 or 1, got {b}"),
            Self::InvalidUtf8 => write!(f, "str value is not valid UTF-8"),
        }
    }
}

impl core::error::Error for CellParseError {}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CustomTypeId(u8);

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
/// | 64–127 | *custom types*                        | per deployment        |                           |                    |
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
            Self::Custom(c) => c.get(),
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
            64..=127 => Ok(Self::Custom(CustomTypeId(id))),
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
            Self::Str | Self::Bytes | Self::Custom(_) => {
                ValueLayout::LengthPrefixed { max: MAX_VALUE_LEN }
            }
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
            (Self::Str, v) if core::str::from_utf8(v).is_err() => Err(CellParseError::InvalidUtf8),
            _ => Ok(()),
        }
    }
}

/// A cell: a type, its value bytes, and whether it is indexable.
///
/// Borrows the value rather than copying it — cells are read straight out of
/// storage buffers, and a `str` or `bytes` value can be up to 256 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell<'a> {
    ty: CellType,
    value: &'a [u8],
    indexable: bool,
}

impl<'a> Cell<'a> {
    /// Build a cell, validating `value` against `ty`.
    pub fn new(ty: CellType, value: &'a [u8], indexable: bool) -> Result<Self, CellParseError> {
        ty.validate(value)?;
        Ok(Self {
            ty,
            value,
            indexable,
        })
    }

    /// Decode exactly one cell from `bytes`, which must hold nothing else.
    ///
    /// Bytes left over are [`CellParseError::TrailingBytes`]; to walk a run of
    /// packed cells use [`Cell::parse_prefix`].
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CellParseError> {
        let (cell, rest) = Self::parse_prefix(bytes)?;
        if rest.is_empty() {
            Ok(cell)
        } else {
            Err(CellParseError::TrailingBytes { extra: rest.len() })
        }
    }

    /// Decode the cell at the front of `bytes`, returning it and whatever
    /// follows. Every cell is self-delimiting, so this is how a run of packed
    /// cells is walked.
    pub fn parse_prefix(bytes: &'a [u8]) -> Result<(Self, &'a [u8]), CellParseError> {
        let (&metadata, rest) = bytes.split_first().ok_or(CellParseError::Empty)?;
        let ty = CellType::from_id(metadata & TYPE_ID_MASK)?;

        let (value, rest) = match ty.layout() {
            ValueLayout::Fixed(n) => {
                if rest.len() < n {
                    return Err(CellParseError::LengthMismatch {
                        expected: n,
                        actual: rest.len(),
                    });
                }
                rest.split_at(n)
            }
            ValueLayout::LengthPrefixed { .. } => {
                let (&len, rest) = rest.split_first().ok_or(CellParseError::MissingLength)?;
                let len = len as usize;
                if rest.len() < len {
                    return Err(CellParseError::Truncated {
                        declared: len,
                        actual: rest.len(),
                    });
                }
                rest.split_at(len)
            }
        };

        let cell = Self::new(ty, value, metadata & INDEXABLE_BIT != 0)?;
        Ok((cell, rest))
    }

    /// Append this cell's wire bytes to `out` — the inverse of
    /// [`Cell::parse`]. Appending several in a row produces a run
    /// [`Cell::parse_prefix`] can walk back.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(2 + self.value.len());
        out.push(self.metadata());
        if self.ty.layout().has_length_byte() {
            // `new`/`parse` cap the value at MAX_VALUE_LEN, so this fits.
            out.push(self.value.len() as u8);
        }
        out.extend_from_slice(self.value);
    }

    /// This cell's wire bytes. [`Cell::encode_into`] avoids the allocation.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// The metadata byte: the indexable bit over the type id.
    pub const fn metadata(&self) -> u8 {
        self.ty.id() | if self.indexable { INDEXABLE_BIT } else { 0 }
    }

    pub const fn cell_type(&self) -> CellType {
        self.ty
    }

    /// The value bytes, without the metadata byte.
    pub const fn value(&self) -> &'a [u8] {
        self.value
    }

    pub const fn is_indexable(&self) -> bool {
        self.indexable
    }

    /// The value as a `str`, if this cell holds one.
    pub fn as_str(&self) -> Option<&'a str> {
        match self.ty {
            CellType::Str => core::str::from_utf8(self.value).ok(),
            _ => None,
        }
    }

    /// The value as a `bool`, if this cell holds one.
    pub fn as_bool(&self) -> Option<bool> {
        match (self.ty, self.value) {
            (CellType::Bool, [b]) => Some(*b != 0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZEROS: [u8; 32] = [0; 32];

    /// One test vector: name, wire bytes, indexable, type, value bytes.
    type Vector = (&'static str, &'static [u8], bool, CellType, &'static [u8]);

    /// One cell per row: its wire bytes, and what they mean. Every core family
    /// appears at least once, at both settings of the indexable bit.
    #[rustfmt::skip]
    const VECTORS: &[Vector] = &[
        ("tombstone",      &[0x00],                          false, CellType::Tombstone,               &[]),
        ("bool false",     &[0x01, 0x00],                    false, CellType::Bool,                    &[0x00]),
        ("bool true, idx", &[0x81, 0x01],                    true,  CellType::Bool,                    &[0x01]),
        ("str empty",      &[0x02, 0x00],                    false, CellType::Str,                     &[]),
        ("str ascii",      &[0x02, 0x02, b'h', b'i'],        false, CellType::Str,                     b"hi"),
        ("str 2-byte utf8",&[0x02, 0x02, 0xC3, 0xA9],        false, CellType::Str,                     &[0xC3, 0xA9]),
        ("str 4-byte utf8",&[0x82, 0x04, 0xF0, 0x9F, 0xA6, 0x80], true, CellType::Str,                 &[0xF0, 0x9F, 0xA6, 0x80]),
        ("bytes empty",    &[0x03, 0x00],                    false, CellType::Bytes,                   &[]),
        ("bytes 2",        &[0x03, 0x02, 0xDE, 0xAD],        false, CellType::Bytes,                   &[0xDE, 0xAD]),
        ("bytes20",        &ZEROS_CELL_20,                   false, CellType::Bytes20,                 &ZEROS20),
        ("bytes4",         &[0x08, 1, 2, 3, 4],              false, CellType::FixedBytes(Width::W4),   &[1, 2, 3, 4]),
        ("bytes8, idx",    &[0x89, 1, 2, 3, 4, 5, 6, 7, 8],  true,  CellType::FixedBytes(Width::W8),   &[1, 2, 3, 4, 5, 6, 7, 8]),
        ("bytes16",        &ZEROS_CELL_16_AT_0A,             false, CellType::FixedBytes(Width::W16),  &ZEROS16),
        ("bytes32",        &ZEROS_CELL_32_AT_0B,             false, CellType::FixedBytes(Width::W32),  &ZEROS),
        ("u32",            &[0x0C, 0, 0, 0, 7],              false, CellType::Uint(Width::W4),         &[0, 0, 0, 7]),
        ("u64, idx",       &[0x8D, 0, 0, 0, 0, 0, 0, 0, 7],  true,  CellType::Uint(Width::W8),         &[0, 0, 0, 0, 0, 0, 0, 7]),
        ("u128",           &ZEROS_CELL_16_AT_0E,             false, CellType::Uint(Width::W16),        &ZEROS16),
        ("u256",           &ZEROS_CELL_32_AT_0F,             false, CellType::Uint(Width::W32),        &ZEROS),
        ("i32",            &[0x10, 0x80, 0, 0, 1],           false, CellType::Int(Width::W4),          &[0x80, 0, 0, 1]),
        ("i64",            &[0x11, 0x80, 0, 0, 0, 0, 0, 0, 1], false, CellType::Int(Width::W8),        &[0x80, 0, 0, 0, 0, 0, 0, 1]),
        ("i128",           &ZEROS_CELL_16_AT_12,             false, CellType::Int(Width::W16),         &ZEROS16),
        ("i256",           &ZEROS_CELL_32_AT_13,             false, CellType::Int(Width::W32),         &ZEROS),
        ("dec32",          &[0x14, 0x80, 0, 0, 1],           false, CellType::Decimal(Width::W4),      &[0x80, 0, 0, 1]),
        ("dec64",          &[0x15, 0, 0, 0, 0, 0, 0, 0, 0],  false, CellType::Decimal(Width::W8),      &[0, 0, 0, 0, 0, 0, 0, 0]),
        ("dec128",         &ZEROS_CELL_16_AT_16,             false, CellType::Decimal(Width::W16),     &ZEROS16),
        ("dec256",         &ZEROS_CELL_32_AT_17,             false, CellType::Decimal(Width::W32),     &ZEROS),
        ("f32",            &[0x18, 0x3F, 0x80, 0, 0],        false, CellType::Float(FloatWidth::F32),  &[0x3F, 0x80, 0, 0]),
        ("f64",            &[0x19, 0x3F, 0xF0, 0, 0, 0, 0, 0, 0], false, CellType::Float(FloatWidth::F64), &[0x3F, 0xF0, 0, 0, 0, 0, 0, 0]),
        ("date32",         &[0x1C, 0x80, 0, 0x4E, 0x20],     false, CellType::Date32,                  &[0x80, 0, 0x4E, 0x20]),
        ("timestamp64",    &[0x9D, 0x80, 0, 0, 0, 0, 0, 0, 1], true, CellType::Timestamp64,            &[0x80, 0, 0, 0, 0, 0, 0, 1]),
        ("custom 64, 0 B", &[0x40, 0x00],                    false, CellType::Custom(CustomTypeId(64)), &[]),
        ("custom 100",     &[0x64, 0x03, 9, 9, 9],           false, CellType::Custom(CustomTypeId(100)), &[9, 9, 9]),
        ("custom 127, idx",&[0xFF, 0x01, 1],                 true,  CellType::Custom(CustomTypeId(127)), &[1]),
    ];

    // The wide vectors, spelled out so the byte strings above stay one line each.
    const ZEROS16: [u8; 16] = [0; 16];
    const ZEROS20: [u8; 20] = [0; 20];
    const ZEROS_CELL_20: [u8; 21] = prepend_20(0x04);
    const ZEROS_CELL_16_AT_0A: [u8; 17] = prepend_16(0x0A);
    const ZEROS_CELL_16_AT_0E: [u8; 17] = prepend_16(0x0E);
    const ZEROS_CELL_16_AT_12: [u8; 17] = prepend_16(0x12);
    const ZEROS_CELL_16_AT_16: [u8; 17] = prepend_16(0x16);
    const ZEROS_CELL_32_AT_0B: [u8; 33] = prepend_32(0x0B);
    const ZEROS_CELL_32_AT_0F: [u8; 33] = prepend_32(0x0F);
    const ZEROS_CELL_32_AT_13: [u8; 33] = prepend_32(0x13);
    const ZEROS_CELL_32_AT_17: [u8; 33] = prepend_32(0x17);

    const fn prepend_16(meta: u8) -> [u8; 17] {
        let mut out = [0u8; 17];
        out[0] = meta;
        out
    }
    const fn prepend_20(meta: u8) -> [u8; 21] {
        let mut out = [0u8; 21];
        out[0] = meta;
        out
    }
    const fn prepend_32(meta: u8) -> [u8; 33] {
        let mut out = [0u8; 33];
        out[0] = meta;
        out
    }

    #[test]
    fn vectors_decode_to_their_stated_meaning() {
        for (name, bytes, indexable, ty, value) in VECTORS {
            let cell = Cell::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cell.cell_type(), *ty, "{name}: type");
            assert_eq!(cell.value(), *value, "{name}: value");
            assert_eq!(cell.is_indexable(), *indexable, "{name}: indexable");
            assert_eq!(cell.metadata(), bytes[0], "{name}: metadata byte");
        }
    }

    #[test]
    fn vectors_re_encode_to_the_same_bytes() {
        for (name, bytes, indexable, ty, value) in VECTORS {
            let cell = Cell::new(*ty, value, *indexable).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cell.encode(), *bytes, "{name}");
        }
    }

    /// The type ids each cover a distinct id, and the set is the spec's.
    #[test]
    fn vectors_cover_every_core_family() {
        let mut seen: Vec<u8> = VECTORS
            .iter()
            .map(|(_, b, ..)| b[0] & TYPE_ID_MASK)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        let expected: Vec<u8> = (0..TYPE_ID_SPACE)
            .filter(|id| CellType::from_id(*id).is_ok() && *id < CUSTOM_TYPE_ID_BASE)
            .collect();
        assert!(
            expected.iter().all(|id| seen.contains(id)),
            "uncovered core ids: {:?}",
            expected
                .iter()
                .filter(|id| !seen.contains(id))
                .collect::<Vec<_>>()
        );
    }

    /// Every id round-trips through `from_id`/`id`, and the reserved slots are
    /// exactly the ones the spec lists.
    #[test]
    fn id_space_matches_the_spec() {
        const RESERVED: &[u8] = &[5, 6, 7, 26, 27, 30, 31];
        for id in 0..TYPE_ID_SPACE {
            let reserved = RESERVED.contains(&id) || (32..64).contains(&id);
            match CellType::from_id(id) {
                Ok(ty) => {
                    assert!(!reserved, "id {id} should be reserved");
                    assert_eq!(ty.id(), id);
                }
                Err(e) => {
                    assert!(reserved, "id {id} should decode");
                    assert_eq!(e, CellParseError::ReservedType(id));
                }
            }
        }
    }

    /// Exhaustive over the whole metadata byte and every value length up to
    /// past the widest fixed type: parsing never panics, a cell that parses
    /// re-encodes to the exact bytes it came from, and acceptance agrees with
    /// the layout table.
    #[test]
    fn every_metadata_byte_and_length() {
        // 0x01 is a valid `bool`, valid UTF-8, and a valid byte anywhere else,
        // so length is the only thing under test.
        let payload = [0x01u8; 40];
        for metadata in 0..=u8::MAX {
            for len in 0..=payload.len() {
                let mut bytes = vec![metadata];
                bytes.extend_from_slice(&payload[..len]);

                let accepted = match CellType::from_id(metadata & TYPE_ID_MASK) {
                    Err(_) => false,
                    Ok(ty) => match ty.layout() {
                        ValueLayout::Fixed(n) => len == n,
                        // The first payload byte is the length byte, and it
                        // must account for every byte after it.
                        ValueLayout::LengthPrefixed { .. } => {
                            len >= 1 && payload[0] as usize == len - 1
                        }
                    },
                };

                match Cell::parse(&bytes) {
                    Ok(cell) => {
                        assert!(accepted, "0x{metadata:02X} len {len}: should have failed");
                        assert_eq!(cell.encode(), bytes, "0x{metadata:02X} len {len}");
                    }
                    Err(_) => assert!(!accepted, "0x{metadata:02X} len {len}: should parse"),
                }
            }
        }
    }

    #[test]
    fn rejects_malformed_cells() {
        let err = |bytes: &[u8]| Cell::parse(bytes).unwrap_err();

        assert_eq!(err(&[]), CellParseError::Empty);
        assert_eq!(err(&[5]), CellParseError::ReservedType(5));
        assert_eq!(err(&[32]), CellParseError::ReservedType(32));
        assert_eq!(
            err(&[CellType::Bytes20.id(), 0, 0]),
            CellParseError::LengthMismatch {
                expected: 20,
                actual: 2
            }
        );
        assert_eq!(
            err(&[CellType::Bool.id(), 2]),
            CellParseError::InvalidBool(2)
        );
        assert_eq!(
            err(&[CellType::Str.id(), 0x01, 0xFF]),
            CellParseError::InvalidUtf8
        );

        // Framing: no length byte, a length byte that overruns, and bytes past
        // the cell's declared end.
        assert_eq!(err(&[CellType::Str.id()]), CellParseError::MissingLength);
        assert_eq!(
            err(&[CellType::Bytes.id(), 4, 1, 2]),
            CellParseError::Truncated {
                declared: 4,
                actual: 2
            }
        );
        assert_eq!(
            err(&[CellType::Bytes.id(), 1, 1, 9, 9]),
            CellParseError::TrailingBytes { extra: 2 }
        );
        assert_eq!(
            err(&[CellType::Bool.id(), 1, 9]),
            CellParseError::TrailingBytes { extra: 1 }
        );

        // A value too long to frame is rejected at construction, since the wire
        // form cannot express it.
        assert_eq!(
            Cell::new(CellType::Str, &[b'a'; MAX_VALUE_LEN + 1], false).unwrap_err(),
            CellParseError::TooLong {
                max: MAX_VALUE_LEN,
                actual: MAX_VALUE_LEN + 1
            }
        );
    }

    /// The point of the length byte: cells pack adjacently and a truncated cell
    /// is rejected rather than read as a shorter valid value.
    #[test]
    fn packed_cells_walk_and_truncation_is_caught() {
        let seven = 7u64.to_be_bytes();
        let cells = [
            Cell::new(CellType::Str, b"hi", true).unwrap(),
            Cell::new(CellType::Uint(Width::W8), &seven, false).unwrap(),
            Cell::new(CellType::Bytes, &[0xDE, 0xAD], false).unwrap(),
            Cell::new(CellType::Tombstone, &[], false).unwrap(),
        ];

        let mut packed = Vec::new();
        for cell in &cells {
            cell.encode_into(&mut packed);
        }

        let mut rest = &packed[..];
        for expected in &cells {
            let (cell, tail) = Cell::parse_prefix(rest).unwrap();
            assert_eq!(cell, *expected);
            rest = tail;
        }
        assert!(rest.is_empty());

        // Cutting the run anywhere never yields the same first cell followed by
        // a clean walk — the truncation is always caught.
        for cut in 1..packed.len() {
            let mut rest = &packed[..cut];
            let walked = std::iter::from_fn(|| match Cell::parse_prefix(rest) {
                Ok((cell, tail)) => {
                    rest = tail;
                    Some(cell)
                }
                Err(_) => None,
            })
            .count();
            assert!(
                walked < cells.len() || !rest.is_empty(),
                "truncating at {cut} still walked the whole run"
            );
        }
    }

    #[test]
    fn typed_accessors() {
        assert_eq!(
            Cell::parse(&[0x02, 0x02, b'h', b'i']).unwrap().as_str(),
            Some("hi")
        );
        assert_eq!(Cell::parse(&[0x01, 1]).unwrap().as_bool(), Some(true));
        assert_eq!(Cell::parse(&[0x01, 0]).unwrap().as_bool(), Some(false));
        // Wrong type: no coercion, no panic.
        assert_eq!(Cell::parse(&[0x01, 1]).unwrap().as_str(), None);
        assert_eq!(Cell::parse(&[0x00]).unwrap().as_bool(), None);
    }

    #[test]
    fn custom_ids_are_the_top_block() {
        assert!(CustomTypeId::new(63).is_none());
        assert_eq!(CustomTypeId::new(64).unwrap().get(), 64);
        assert_eq!(CustomTypeId::new(127).unwrap().get(), 127);
        assert!(CustomTypeId::new(128).is_none());
        // A custom type is framed like any other variable-width one, so a cell
        // carrying it stays self-delimiting even though its content is opaque.
        assert_eq!(
            CellType::from_id(100).unwrap().layout(),
            ValueLayout::LengthPrefixed { max: MAX_VALUE_LEN }
        );
    }
}

/// Properties that must hold over generated inputs, rather than over the fixed
/// vectors above. These cover what [`tests::every_metadata_byte_and_length`]
/// cannot: arbitrary *content*, and lengths past the variable-width maximum.
#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;

    /// Every type in the table, each width included.
    fn any_cell_type() -> impl Strategy<Value = CellType> {
        (0u8..TYPE_ID_SPACE).prop_filter_map("reserved id", |id| CellType::from_id(id).ok())
    }

    /// A type paired with a value whose *content* is valid for it. Lengths
    /// deliberately straddle the variable-width maximum, so `TooLong` is the one
    /// error [`build_encode_parse_round_trips`] may see.
    fn any_valid_cell() -> impl Strategy<Value = (CellType, Vec<u8>)> {
        any_cell_type().prop_flat_map(|ty| {
            let value = match ty {
                // Content-constrained: not every byte string of the right
                // length is a valid value.
                CellType::Bool => prop::collection::vec(0u8..=1, 1..=1).boxed(),
                CellType::Str => prop::string::string_regex(".{0,120}")
                    .unwrap()
                    .prop_map(String::into_bytes)
                    .boxed(),
                // Length is the only constraint.
                _ => match ty.layout() {
                    ValueLayout::Fixed(n) => prop::collection::vec(any::<u8>(), n..=n).boxed(),
                    ValueLayout::LengthPrefixed { max } => {
                        prop::collection::vec(any::<u8>(), 0..=max + 8).boxed()
                    }
                },
            };
            (Just(ty), value)
        })
    }

    proptest! {
        /// Parsing arbitrary bytes never panics, and whatever parses re-encodes
        /// to the exact bytes it came from.
        #[test]
        fn parse_is_total_and_encode_inverts_it(bytes in prop::collection::vec(any::<u8>(), 0..600)) {
            if let Ok(cell) = Cell::parse(&bytes) {
                prop_assert_eq!(cell.encode(), bytes);
            }
        }

        /// A cell built from a valid (type, value) survives encode → parse
        /// unchanged, at both settings of the indexable bit.
        #[test]
        fn build_encode_parse_round_trips(
            (ty, value) in any_valid_cell(),
            indexable in any::<bool>(),
        ) {
            let built = match Cell::new(ty, &value, indexable) {
                Ok(cell) => cell,
                // The generator straddles the variable-width maximum on
                // purpose; an over-long value must be rejected, not encoded.
                Err(e) => {
                    let too_long = matches!(e, CellParseError::TooLong { .. });
                    prop_assert!(too_long, "expected TooLong, got {}", e);
                    return Ok(());
                }
            };
            let encoded = built.encode();
            let parsed = Cell::parse(&encoded).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(parsed, built);
            prop_assert_eq!(parsed.cell_type(), ty);
            prop_assert_eq!(parsed.value(), &value[..]);
            prop_assert_eq!(parsed.is_indexable(), indexable);
        }

        /// The indexable bit and the type id never bleed into each other.
        #[test]
        fn metadata_byte_splits_cleanly(ty in any_cell_type(), indexable in any::<bool>()) {
            let value = vec![0u8; match ty.layout() {
                ValueLayout::Fixed(n) => n,
                _ => 0,
            }];
            let cell = Cell::new(ty, &value, indexable).unwrap();
            let metadata = cell.metadata();
            prop_assert_eq!(metadata & TYPE_ID_MASK, ty.id());
            prop_assert_eq!(metadata & INDEXABLE_BIT != 0, indexable);
        }

        /// A `str` cell parses exactly when its bytes are UTF-8 — the parser
        /// agrees with the standard library, not with its own idea of UTF-8.
        #[test]
        fn str_accepts_exactly_utf8(value in prop::collection::vec(any::<u8>(), 0..=MAX_VALUE_LEN)) {
            let cell = [&[CellType::Str.id(), value.len() as u8][..], &value].concat();
            let parsed = Cell::parse(&cell);
            prop_assert_eq!(parsed.is_ok(), core::str::from_utf8(&value).is_ok());
            if let Ok(cell) = parsed {
                prop_assert_eq!(cell.as_str(), Some(core::str::from_utf8(&value).unwrap()));
            }
        }

        /// A run of cells packs and walks back unchanged, and cutting the run
        /// short is always caught rather than read as a shorter value.
        #[test]
        fn packed_runs_round_trip(cells in prop::collection::vec(any_valid_cell(), 0..8)) {
            let cells: Vec<_> = cells
                .iter()
                .filter_map(|(ty, v)| Cell::new(*ty, v, false).ok())
                .collect();

            let mut packed = Vec::new();
            for cell in &cells {
                cell.encode_into(&mut packed);
            }

            let mut rest = &packed[..];
            for expected in &cells {
                let (cell, tail) = Cell::parse_prefix(rest)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                prop_assert_eq!(cell, *expected);
                rest = tail;
            }
            prop_assert!(rest.is_empty());
        }

        /// Reserved ids stay reserved whatever follows them.
        #[test]
        fn reserved_ids_never_parse(
            id in (0u8..TYPE_ID_SPACE).prop_filter("valid id", |id| CellType::from_id(*id).is_err()),
            indexable in any::<bool>(),
            tail in prop::collection::vec(any::<u8>(), 0..40),
        ) {
            let metadata = id | if indexable { INDEXABLE_BIT } else { 0 };
            let cell = [&[metadata][..], &tail].concat();
            prop_assert_eq!(Cell::parse(&cell), Err(CellParseError::ReservedType(id)));
            prop_assert_eq!(Cell::parse_prefix(&cell).err(), Some(CellParseError::ReservedType(id)));
        }
    }
}
