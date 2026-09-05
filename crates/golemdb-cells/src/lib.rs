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

/// How many bytes [`CellParseError::InvalidUtf8`] quotes back. Enough to show
/// the longest UTF-8 sequence and its neighbours, short enough for a log line.
pub const UTF8_SNIPPET_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellParseError {
    /// The cell is too short to hold its metadata byte.
    Empty,
    /// The type id names a slot the spec reserves for future use.
    ReservedType(u8),
    /// A variable-width cell that stops before its length byte.
    MissingLength,
    /// A fixed-width type whose value is not exactly that wide.
    LengthMismatch {
        ty: CellType,
        expected: usize,
        actual: usize,
    },
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
    /// A `str` whose bytes are not valid UTF-8, with a window onto the bytes
    /// that failed. Build one with [`CellParseError::invalid_utf8`].
    ///
    /// The window is copied inline rather than borrowed or boxed, so the error
    /// stays `Copy` and rejecting a cell costs no allocation — this parses
    /// untrusted input, so the reject path is the hot one under attack.
    InvalidUtf8 {
        /// How many bytes were valid before the failure.
        valid_up_to: usize,
        /// The value's full length, which `snippet` may not cover.
        total: usize,
        /// Up to [`UTF8_SNIPPET_LEN`] bytes starting at `valid_up_to`, so the
        /// window shows the failure rather than the start of a long value.
        snippet: [u8; UTF8_SNIPPET_LEN],
        /// How much of `snippet` is real; the rest is zero padding.
        snippet_len: u8,
    },
    /// The type id is in the custom block (64–127) but this build has the
    /// `custom_types` feature off, so it has no way to interpret the cell.
    ///
    /// Defined whether or not the feature is on, so that turning it on does not
    /// change the shape of this enum for anything matching on it.
    CustomTypesDisabled(u8),
}

impl fmt::Display for CellParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "cell is missing its metadata byte"),
            Self::ReservedType(id) => write!(f, "type id {id} is reserved"),
            Self::MissingLength => write!(f, "cell is missing its length byte"),
            Self::LengthMismatch {
                ty,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "{} expects {expected} value bytes, got {actual}",
                    ty.name()
                )
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
            Self::InvalidUtf8 {
                valid_up_to,
                total,
                snippet,
                snippet_len,
            } => {
                write!(
                    f,
                    "str is not valid UTF-8 at byte {valid_up_to} of {total}: "
                )?;
                let len = *snippet_len as usize;
                for (i, b) in snippet[..len].iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{b:02x}")?;
                }
                if valid_up_to + len < *total {
                    write!(f, " …")?;
                }
                Ok(())
            }
            Self::CustomTypesDisabled(id) => {
                write!(f, "type id {id} is custom; the custom_types feature is off")
            }
        }
    }
}

impl core::error::Error for CellParseError {}

impl CellParseError {
    /// The [`InvalidUtf8`](CellParseError::InvalidUtf8) for `value`, whose first
    /// `valid_up_to` bytes decoded before it went wrong.
    ///
    /// `const` so test vectors and other tables can name the expected error
    /// without spelling out the padded snippet array.
    pub const fn invalid_utf8(value: &[u8], valid_up_to: usize) -> Self {
        let mut snippet = [0u8; UTF8_SNIPPET_LEN];
        let mut i = 0;
        // A plain loop rather than `copy_from_slice`, which is not const.
        while i < UTF8_SNIPPET_LEN && valid_up_to + i < value.len() {
            snippet[i] = value[valid_up_to + i];
            i += 1;
        }
        Self::InvalidUtf8 {
            valid_up_to,
            total: value.len(),
            snippet,
            snippet_len: i as u8,
        }
    }
}

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

/// A cell: a type, its value bytes, and whether it is indexable.
///
/// Borrows the value rather than copying it — cells are read straight out of
/// storage buffers, and a `str` or `bytes` value can be up to 256 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellValue<'a> {
    ty: CellType,
    value: &'a [u8],
    indexable: bool,
}

impl<'a> CellValue<'a> {
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
                        ty,
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

    /// The value as the raw bytes of `want`, or `None` if this cell holds some
    /// other type. The width comes from `want`, so a mismatch cannot compile
    /// into a silent reinterpretation.
    fn exact<const N: usize>(&self, want: CellType) -> Option<[u8; N]> {
        if self.ty != want {
            return None;
        }
        // Infallible: `validate` already fixed the width for a fixed-width
        // type, and every caller below asks for that type's own width.
        self.value.try_into().ok()
    }

    // -- byte strings ------------------------------------------------------

    /// The value of a `bytes` cell. For a fixed-width byte string use
    /// [`as_bytes4`](Self::as_bytes4) and friends, which give a sized array.
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        (self.ty == CellType::Bytes).then_some(self.value)
    }

    pub fn as_bytes4(&self) -> Option<[u8; 4]> {
        self.exact(CellType::FixedBytes(Width::W4))
    }

    pub fn as_bytes8(&self) -> Option<[u8; 8]> {
        self.exact(CellType::FixedBytes(Width::W8))
    }

    pub fn as_bytes16(&self) -> Option<[u8; 16]> {
        self.exact(CellType::FixedBytes(Width::W16))
    }

    /// An address-width byte string.
    pub fn as_bytes20(&self) -> Option<[u8; 20]> {
        self.exact(CellType::Bytes20)
    }

    pub fn as_bytes32(&self) -> Option<[u8; 32]> {
        self.exact(CellType::FixedBytes(Width::W32))
    }

    // -- unsigned integers, big-endian -------------------------------------

    pub fn as_u32(&self) -> Option<u32> {
        self.exact(CellType::Uint(Width::W4))
            .map(u32::from_be_bytes)
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.exact(CellType::Uint(Width::W8))
            .map(u64::from_be_bytes)
    }

    pub fn as_u128(&self) -> Option<u128> {
        self.exact(CellType::Uint(Width::W16))
            .map(u128::from_be_bytes)
    }

    /// A `u256` as its 32 big-endian bytes — Rust has no `u256`, so widening it
    /// into one is the caller's job (`alloy_primitives::U256::from_be_bytes`).
    pub fn as_u256_be(&self) -> Option<[u8; 32]> {
        self.exact(CellType::Uint(Width::W32))
    }

    // -- signed integers, two's complement big-endian ----------------------
    //
    // Plain two's complement, not the sign-biased form the order-encoding
    // column describes: that bias belongs to index keys, not to stored values.

    pub fn as_i32(&self) -> Option<i32> {
        self.exact(CellType::Int(Width::W4)).map(i32::from_be_bytes)
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.exact(CellType::Int(Width::W8)).map(i64::from_be_bytes)
    }

    pub fn as_i128(&self) -> Option<i128> {
        self.exact(CellType::Int(Width::W16))
            .map(i128::from_be_bytes)
    }

    /// An `i256` as its 32 big-endian bytes; see [`as_u256_be`](Self::as_u256_be).
    pub fn as_i256_be(&self) -> Option<[u8; 32]> {
        self.exact(CellType::Int(Width::W32))
    }

    // -- decimals ----------------------------------------------------------
    //
    // These return the *unscaled* mantissa. The spec pins a fixed scale per
    // width, but that scale is not represented in this crate yet, so applying
    // it is the caller's job — see the note on `CellType::Decimal`.

    pub fn as_dec32_unscaled(&self) -> Option<i32> {
        self.exact(CellType::Decimal(Width::W4))
            .map(i32::from_be_bytes)
    }

    pub fn as_dec64_unscaled(&self) -> Option<i64> {
        self.exact(CellType::Decimal(Width::W8))
            .map(i64::from_be_bytes)
    }

    pub fn as_dec128_unscaled(&self) -> Option<i128> {
        self.exact(CellType::Decimal(Width::W16))
            .map(i128::from_be_bytes)
    }

    /// A `dec256` mantissa as its 32 big-endian bytes.
    pub fn as_dec256_unscaled_be(&self) -> Option<[u8; 32]> {
        self.exact(CellType::Decimal(Width::W32))
    }

    // -- floats, IEEE-754 big-endian ---------------------------------------

    pub fn as_f32(&self) -> Option<f32> {
        self.exact(CellType::Float(FloatWidth::F32))
            .map(f32::from_be_bytes)
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.exact(CellType::Float(FloatWidth::F64))
            .map(f64::from_be_bytes)
    }

    // -- time --------------------------------------------------------------

    /// Days since the Unix epoch, signed.
    pub fn as_date32(&self) -> Option<i32> {
        self.exact(CellType::Date32).map(i32::from_be_bytes)
    }

    /// Microseconds since the Unix epoch, signed.
    pub fn as_timestamp64(&self) -> Option<i64> {
        self.exact(CellType::Timestamp64).map(i64::from_be_bytes)
    }

    // -- custom ------------------------------------------------------------

    /// A custom cell's id and its bytes, which this layer does not interpret.
    #[cfg(feature = "custom_types")]
    pub fn as_custom(&self) -> Option<(CustomTypeId, &'a [u8])> {
        match self.ty {
            CellType::Custom(id) => Some((id, self.value)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test vector: name, wire bytes, indexable, type, value bytes.
    type Vector = (&'static str, &'static [u8], bool, CellType, &'static [u8]);

    /// One cell per row: its wire bytes, and what they mean. Every core family
    /// appears at least once, at both settings of the indexable bit.
    #[rustfmt::skip]
    const VECTORS: &[Vector] = &[
        // name              wire bytes                             idx    type                              value
        ("tombstone",       &[0x00],                               false, CellType::Tombstone,              &[]),
        ("bool false",      &[0x01, 0x00],                         false, CellType::Bool,                   &[0x00]),
        ("bool true, idx",  &[0x81, 0x01],                         true,  CellType::Bool,                   &[0x01]),
        ("str empty",       &[0x02, 0x00],                         false, CellType::Str,                    &[]),
        ("str ascii",       &[0x02, 0x02, b'h', b'i'],             false, CellType::Str,                    b"hi"),
        ("str 2-byte utf8", &[0x02, 0x02, 0xC3, 0xA9],             false, CellType::Str,                    &[0xC3, 0xA9]),
        ("str 4-byte utf8", &[0x82, 0x04, 0xF0, 0x9F, 0xA6, 0x80], true,  CellType::Str,                    &[0xF0, 0x9F, 0xA6, 0x80]),
        // The accept side of the UTF-8 boundary BAD_VECTORS attacks: an
        // embedded NUL is valid UTF-8, and these are the last code points
        // before each rejected range.
        ("str with a NUL",  &[0x02, 0x03, b'a', 0x00, b'b'],       false, CellType::Str,                    &[b'a', 0x00, b'b']),
        ("str U+D7FF",      &[0x02, 0x03, 0xED, 0x9F, 0xBF],       false, CellType::Str,                    &[0xED, 0x9F, 0xBF]),
        ("str U+FFFF",      &[0x02, 0x03, 0xEF, 0xBF, 0xBF],       false, CellType::Str,                    &[0xEF, 0xBF, 0xBF]),
        ("str U+10FFFF",    &[0x02, 0x04, 0xF4, 0x8F, 0xBF, 0xBF], false, CellType::Str,                    &[0xF4, 0x8F, 0xBF, 0xBF]),
        ("bytes empty",     &[0x03, 0x00],                         false, CellType::Bytes,                  &[]),
        ("bytes 2",         &[0x03, 0x02, 0xDE, 0xAD],             false, CellType::Bytes,                  &[0xDE, 0xAD]),
        ("bytes20",         &BYTES20_ZERO_CELL,                    false, CellType::Bytes20,                &ZERO_VALUE_20),
        ("bytes4",          &[0x08, 1, 2, 3, 4],                   false, CellType::FixedBytes(Width::W4),  &[1, 2, 3, 4]),
        ("bytes8, idx",     &[0x89, 1, 2, 3, 4, 5, 6, 7, 8],       true,  CellType::FixedBytes(Width::W8),  &[1, 2, 3, 4, 5, 6, 7, 8]),
        ("bytes16",         &BYTES16_ZERO_CELL,                    false, CellType::FixedBytes(Width::W16), &ZERO_VALUE_16),
        ("bytes32",         &BYTES32_ZERO_CELL,                    false, CellType::FixedBytes(Width::W32), &ZERO_VALUE_32),
        ("u32",             &[0x0C, 0, 0, 0, 7],                   false, CellType::Uint(Width::W4),        &[0, 0, 0, 7]),
        ("u64, idx",        &[0x8D, 0, 0, 0, 0, 0, 0, 0, 7],       true,  CellType::Uint(Width::W8),        &[0, 0, 0, 0, 0, 0, 0, 7]),
        ("u128",            &U128_ZERO_CELL,                       false, CellType::Uint(Width::W16),       &ZERO_VALUE_16),
        ("u256",            &U256_ZERO_CELL,                       false, CellType::Uint(Width::W32),       &ZERO_VALUE_32),
        ("i32",             &[0x10, 0x80, 0, 0, 1],                false, CellType::Int(Width::W4),         &[0x80, 0, 0, 1]),
        ("i64",             &[0x11, 0x80, 0, 0, 0, 0, 0, 0, 1],    false, CellType::Int(Width::W8),         &[0x80, 0, 0, 0, 0, 0, 0, 1]),
        ("i128",            &I128_ZERO_CELL,                       false, CellType::Int(Width::W16),        &ZERO_VALUE_16),
        ("i256",            &I256_ZERO_CELL,                       false, CellType::Int(Width::W32),        &ZERO_VALUE_32),
        ("dec32",           &[0x14, 0x80, 0, 0, 1],                false, CellType::Decimal(Width::W4),     &[0x80, 0, 0, 1]),
        ("dec64",           &[0x15, 0, 0, 0, 0, 0, 0, 0, 0],       false, CellType::Decimal(Width::W8),     &[0, 0, 0, 0, 0, 0, 0, 0]),
        ("dec128",          &DEC128_ZERO_CELL,                     false, CellType::Decimal(Width::W16),    &ZERO_VALUE_16),
        ("dec256",          &DEC256_ZERO_CELL,                     false, CellType::Decimal(Width::W32),    &ZERO_VALUE_32),
        ("f32",             &[0x18, 0x3F, 0x80, 0, 0],             false, CellType::Float(FloatWidth::F32), &[0x3F, 0x80, 0, 0]),
        ("f64",             &[0x19, 0x3F, 0xF0, 0, 0, 0, 0, 0, 0], false, CellType::Float(FloatWidth::F64), &[0x3F, 0xF0, 0, 0, 0, 0, 0, 0]),
        ("date32",          &[0x1C, 0x80, 0, 0x4E, 0x20],          false, CellType::Date32,                 &[0x80, 0, 0x4E, 0x20]),
        ("timestamp64",     &[0x9D, 0x80, 0, 0, 0, 0, 0, 0, 1],    true,  CellType::Timestamp64,            &[0x80, 0, 0, 0, 0, 0, 0, 1]),
    ];

    /// The custom block's vectors, present only when the feature that decodes
    /// those ids is on. Empty otherwise, so every test below reads the same.
    #[cfg(feature = "custom_types")]
    #[rustfmt::skip]
    const CUSTOM_VECTORS: &[Vector] = &[
        ("custom 64, 0 B", &[0x40, 0x00],                    false, CellType::Custom(CustomTypeId(64)), &[]),
        ("custom 100",     &[0x64, 0x03, 9, 9, 9],           false, CellType::Custom(CustomTypeId(100)), &[9, 9, 9]),
        ("custom 127, idx",&[0xFF, 0x01, 1],                 true,  CellType::Custom(CustomTypeId(127)), &[1]),
    ];

    #[cfg(not(feature = "custom_types"))]
    const CUSTOM_VECTORS: &[Vector] = &[];

    /// Every accept vector that applies to this build.
    fn accept_vectors() -> impl Iterator<Item = &'static Vector> {
        VECTORS.iter().chain(CUSTOM_VECTORS)
    }

    // The 16-, 20- and 32-byte vectors, lifted out of the table above so its
    // rows stay one line each. Every one is an all-zero value of a fixed-width
    // type, so there is no length byte and the value is the whole tail.
    //
    // `<TYPE>_ZERO` is the value; `<TYPE>_ZERO_CELL` is that value with its
    // metadata byte in front.
    const ZERO_VALUE_16: [u8; 16] = [0; 16];
    const ZERO_VALUE_20: [u8; 20] = [0; 20];
    const ZERO_VALUE_32: [u8; 32] = [0; 32];

    const BYTES20_ZERO_CELL: [u8; 21] = zero_cell_20(CellType::Bytes20);

    const BYTES16_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::FixedBytes(Width::W16));
    const U128_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::Uint(Width::W16));
    const I128_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::Int(Width::W16));
    const DEC128_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::Decimal(Width::W16));

    const BYTES32_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::FixedBytes(Width::W32));
    const U256_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::Uint(Width::W32));
    const I256_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::Int(Width::W32));
    const DEC256_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::Decimal(Width::W32));

    /// A whole cell of a 16-byte-wide type: `ty`'s metadata byte, then a
    /// 16-byte zero value.
    ///
    /// Taking a [`CellType`] rather than a raw byte is what keeps the constants
    /// above readable — the type is named, not spelled as a hex id. One
    /// function per width, because an array's length is part of its type and a
    /// `const fn` cannot be generic over it here.
    const fn zero_cell_16(ty: CellType) -> [u8; 17] {
        let mut out = [0u8; 17];
        out[0] = ty.id();
        out
    }

    /// A whole cell of a 20-byte-wide type. See [`zero_cell_16`].
    const fn zero_cell_20(ty: CellType) -> [u8; 21] {
        let mut out = [0u8; 21];
        out[0] = ty.id();
        out
    }

    /// A whole cell of a 32-byte-wide type. See [`zero_cell_16`].
    const fn zero_cell_32(ty: CellType) -> [u8; 33] {
        let mut out = [0u8; 33];
        out[0] = ty.id();
        out
    }

    #[test]
    fn vectors_decode_to_their_stated_meaning() {
        for (name, bytes, indexable, ty, value) in accept_vectors() {
            let cell = CellValue::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cell.cell_type(), *ty, "{name}: type");
            assert_eq!(cell.value(), *value, "{name}: value");
            assert_eq!(cell.is_indexable(), *indexable, "{name}: indexable");
            assert_eq!(cell.metadata(), bytes[0], "{name}: metadata byte");
        }
    }

    #[test]
    fn vectors_re_encode_to_the_same_bytes() {
        for (name, bytes, indexable, ty, value) in accept_vectors() {
            let cell =
                CellValue::new(*ty, value, *indexable).unwrap_or_else(|e| panic!("{name}: {e}"));
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

    /// Names are the spec table's, id for id — what `LengthMismatch` and other
    /// diagnostics print.
    #[test]
    fn type_names_match_the_spec() {
        #[rustfmt::skip]
        const NAMES: &[(u8, &str)] = &[
            (0, "tombstone"), (1, "bool"), (2, "str"), (3, "bytes"), (4, "bytes20"),
            (8, "bytes4"), (9, "bytes8"), (10, "bytes16"), (11, "bytes32"),
            (12, "u32"), (13, "u64"), (14, "u128"), (15, "u256"),
            (16, "i32"), (17, "i64"), (18, "i128"), (19, "i256"),
            (20, "dec32"), (21, "dec64"), (22, "dec128"), (23, "dec256"),
            (24, "f32"), (25, "f64"), (28, "date32"), (29, "timestamp64"),
        ];

        for (id, name) in NAMES {
            assert_eq!(CellType::from_id(*id).unwrap().name(), *name, "id {id}");
        }

        // Every core id is named, and no two share a name.
        let named: Vec<u8> = NAMES.iter().map(|(id, _)| *id).collect();
        for id in 0..CUSTOM_TYPE_ID_BASE {
            assert_eq!(
                CellType::from_id(id).is_ok(),
                named.contains(&id),
                "id {id}"
            );
        }
        let mut names: Vec<&str> = NAMES.iter().map(|(_, n)| *n).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two types share a name");
    }

    /// Every id round-trips through `from_id`/`id`, and the reserved slots are
    /// exactly the ones the spec lists.
    #[test]
    fn id_space_matches_the_spec() {
        const RESERVED: &[u8] = &[5, 6, 7, 26, 27, 30, 31];
        // With `custom_types` off the top block decodes to nothing either, but
        // as `CustomTypesDisabled` rather than `ReservedType`.
        let custom_off = cfg!(not(feature = "custom_types"));
        for id in 0..TYPE_ID_SPACE {
            let reserved = RESERVED.contains(&id) || (32..64).contains(&id);
            let disabled = custom_off && id >= CUSTOM_TYPE_ID_BASE;
            match CellType::from_id(id) {
                Ok(ty) => {
                    assert!(!reserved && !disabled, "id {id} should not decode");
                    assert_eq!(ty.id(), id);
                }
                Err(e) if disabled => {
                    assert_eq!(e, CellParseError::CustomTypesDisabled(id));
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

                match CellValue::parse(&bytes) {
                    Ok(cell) => {
                        assert!(accepted, "0x{metadata:02X} len {len}: should have failed");
                        assert_eq!(cell.encode(), bytes, "0x{metadata:02X} len {len}");
                    }
                    Err(_) => assert!(!accepted, "0x{metadata:02X} len {len}: should parse"),
                }
            }
        }
    }

    /// One rejection vector: name, wire bytes, and the error they must produce.
    ///
    /// The error is asserted exactly, not just "it failed" — a cell rejected
    /// for the wrong reason is a bug the way a cell accepted wrongly is.
    type BadVector = (&'static str, &'static [u8], CellParseError);

    /// [`CellParseError::LengthMismatch`] as a call rather than a struct
    /// literal, so the rows below stay one line each and stay aligned.
    const fn length_mismatch(ty: CellType, expected: usize, actual: usize) -> CellParseError {
        CellParseError::LengthMismatch {
            ty,
            expected,
            actual,
        }
    }

    #[rustfmt::skip]
    const BAD_VECTORS: &[BadVector] = &[
        // name                      wire bytes                                expected error

        // -- nothing to parse ---------------------------------------------
        ("empty slice",             &[],                                      CellParseError::Empty),

        // -- reserved type ids, one per reserved block --------------------
        ("reserved singleton 5",    &[5],                                     CellParseError::ReservedType(5)),
        ("reserved singleton 7",    &[7],                                     CellParseError::ReservedType(7)),
        ("reserved float 26",       &[26],                                    CellParseError::ReservedType(26)),
        ("reserved time 30",        &[30],                                    CellParseError::ReservedType(30)),
        ("reserved family 32",      &[32],                                    CellParseError::ReservedType(32)),
        ("reserved family 63",      &[63],                                    CellParseError::ReservedType(63)),
        ("reserved, idx bit set",   &[0x80 | 5],                              CellParseError::ReservedType(5)),
        ("reserved with payload",   &[32, 1, 2, 3],                           CellParseError::ReservedType(32)),

        // -- fixed-width framing ------------------------------------------
        ("bytes20 too short",       &[0x04, 0, 0],                            length_mismatch(CellType::Bytes20, 20, 2)),
        ("u32 one byte short",      &[0x0C, 0, 0, 0],                         length_mismatch(CellType::Uint(Width::W4), 4, 3)),
        ("u64 empty",               &[0x0D],                                  length_mismatch(CellType::Uint(Width::W8), 8, 0)),
        ("bool empty",              &[0x01],                                  length_mismatch(CellType::Bool, 1, 0)),
        ("bool with a spare byte",  &[0x01, 1, 9],                            CellParseError::TrailingBytes { extra: 1 }),
        ("tombstone with a value",  &[0x00, 9],                               CellParseError::TrailingBytes { extra: 1 }),
        ("u32 one byte over",       &[0x0C, 0, 0, 0, 7, 9],                   CellParseError::TrailingBytes { extra: 1 }),

        // -- variable-width framing ---------------------------------------
        ("str, no length byte",     &[0x02],                                  CellParseError::MissingLength),
        ("bytes, no length byte",   &[0x03],                                  CellParseError::MissingLength),
        #[cfg(feature = "custom_types")]
        ("custom, no length byte",  &[0x40],                                  CellParseError::MissingLength),
        ("str cut short",           &[0x02, 4, b'h', b'i'],                   CellParseError::Truncated { declared: 4, actual: 2 }),
        ("bytes cut short",         &[0x03, 4, 1, 2],                         CellParseError::Truncated { declared: 4, actual: 2 }),
        #[cfg(feature = "custom_types")]
        ("custom cut short",        &[0x64, 3, 9],                            CellParseError::Truncated { declared: 3, actual: 1 }),
        ("len 0 but bytes follow",  &[0x03, 0, 9, 9],                         CellParseError::TrailingBytes { extra: 2 }),
        ("bytes, extra past end",   &[0x03, 1, 1, 9, 9],                      CellParseError::TrailingBytes { extra: 2 }),

        // -- the custom block with the feature off -------------------------
        #[cfg(not(feature = "custom_types"))]
        ("custom 64, feature off",  &[0x40, 0x00],                            CellParseError::CustomTypesDisabled(64)),
        #[cfg(not(feature = "custom_types"))]
        ("custom 100, feature off", &[0x64, 0x03, 9, 9, 9],                   CellParseError::CustomTypesDisabled(100)),
        #[cfg(not(feature = "custom_types"))]
        ("custom 127, feature off", &[0xFF, 0x01, 1],                         CellParseError::CustomTypesDisabled(127)),

        // -- content: bool -------------------------------------------------
        ("bool byte 2",             &[0x01, 2],                               CellParseError::InvalidBool(2)),
        ("bool byte 0xFF",          &[0x01, 0xFF],                            CellParseError::InvalidBool(0xFF)),

        // -- content: str that is not really UTF-8 -------------------------
        // Each is correctly framed, so only the content is under test.
        ("lone continuation 0x80",  &[0x02, 1, 0x80],                         CellParseError::invalid_utf8(&[0x80], 0)),
        ("continuation mid-str",    &[0x02, 3, b'h', 0x80, b'i'],             CellParseError::invalid_utf8(&[b'h', 0x80, b'i'], 1)),
        ("0xFF, never in UTF-8",    &[0x02, 1, 0xFF],                         CellParseError::invalid_utf8(&[0xFF], 0)),
        ("0xFE, never in UTF-8",    &[0x02, 1, 0xFE],                         CellParseError::invalid_utf8(&[0xFE], 0)),
        // A lead byte promising more bytes than the value carries.
        ("2-byte lead, no tail",    &[0x02, 1, 0xC3],                         CellParseError::invalid_utf8(&[0xC3], 0)),
        ("3-byte lead, one tail",   &[0x02, 2, 0xE2, 0x82],                   CellParseError::invalid_utf8(&[0xE2, 0x82], 0)),
        ("4-byte lead, two tails",  &[0x02, 3, 0xF0, 0x9F, 0xA6],             CellParseError::invalid_utf8(&[0xF0, 0x9F, 0xA6], 0)),
        // Overlong forms: a code point encoded in more bytes than needed. The
        // classic filter bypass — "/" and NUL smuggled past a byte comparison.
        ("overlong '/' (C0 AF)",    &[0x02, 2, 0xC0, 0xAF],                   CellParseError::invalid_utf8(&[0xC0, 0xAF], 0)),
        ("overlong NUL (C0 80)",    &[0x02, 2, 0xC0, 0x80],                   CellParseError::invalid_utf8(&[0xC0, 0x80], 0)),
        ("overlong 3-byte NUL",     &[0x02, 3, 0xE0, 0x80, 0x80],             CellParseError::invalid_utf8(&[0xE0, 0x80, 0x80], 0)),
        // UTF-16 surrogate halves are not scalar values, so not valid UTF-8.
        ("surrogate U+D800",        &[0x02, 3, 0xED, 0xA0, 0x80],             CellParseError::invalid_utf8(&[0xED, 0xA0, 0x80], 0)),
        ("surrogate U+DFFF",        &[0x02, 3, 0xED, 0xBF, 0xBF],             CellParseError::invalid_utf8(&[0xED, 0xBF, 0xBF], 0)),
        // Past U+10FFFF, and the 5-byte forms UTF-8 never had.
        ("beyond U+10FFFF",         &[0x02, 4, 0xF5, 0x80, 0x80, 0x80],       CellParseError::invalid_utf8(&[0xF5, 0x80, 0x80, 0x80], 0)),
        ("5-byte sequence",         &[0x02, 5, 0xF8, 0x88, 0x80, 0x80, 0x80], CellParseError::invalid_utf8(&[0xF8, 0x88, 0x80, 0x80, 0x80], 0)),
        // Framing is right but the length byte cuts a character in half — the
        // case a length byte alone cannot catch, and UTF-8 validation does.
        ("len splits a character",  &[0x02, 1, 0xC3, 0xA9],                   CellParseError::invalid_utf8(&[0xC3], 0)),
    ];

    #[test]
    fn bad_vectors_are_rejected_for_the_stated_reason() {
        for (name, bytes, expected) in BAD_VECTORS {
            let got = CellValue::parse(bytes)
                .map(|c| c.cell_type())
                .expect_err(&format!("{name}: parsed, should have failed"));
            assert_eq!(got, *expected, "{name}");
        }
    }

    /// Each accessor decodes its own type's value.
    #[test]
    fn accessors_decode_their_rust_equivalents() {
        fn parse(bytes: &[u8]) -> CellValue<'_> {
            CellValue::parse(bytes).unwrap()
        }

        assert_eq!(parse(&[0x01, 1]).as_bool(), Some(true));
        assert_eq!(parse(&[0x01, 0]).as_bool(), Some(false));
        assert_eq!(parse(&[0x02, 2, b'h', b'i']).as_str(), Some("hi"));
        assert_eq!(
            parse(&[0x03, 2, 0xDE, 0xAD]).as_bytes(),
            Some(&[0xDE, 0xAD][..])
        );

        assert_eq!(parse(&[0x08, 1, 2, 3, 4]).as_bytes4(), Some([1, 2, 3, 4]));
        assert_eq!(
            parse(&[&[0x04][..], &[7u8; 20]].concat()).as_bytes20(),
            Some([7u8; 20])
        );
        assert_eq!(
            parse(&[&[0x0B][..], &[9u8; 32]].concat()).as_bytes32(),
            Some([9u8; 32])
        );

        assert_eq!(
            parse(&[&[0x0C][..], &7u32.to_be_bytes()].concat()).as_u32(),
            Some(7)
        );
        assert_eq!(
            parse(&[&[0x0D][..], &u64::MAX.to_be_bytes()].concat()).as_u64(),
            Some(u64::MAX)
        );
        assert_eq!(
            parse(&[&[0x0E][..], &1u128.to_be_bytes()].concat()).as_u128(),
            Some(1)
        );
        assert_eq!(
            parse(&[&[0x0F][..], &[0xFFu8; 32]].concat()).as_u256_be(),
            Some([0xFFu8; 32])
        );

        // Signed values are plain two's complement, so negatives round-trip.
        assert_eq!(
            parse(&[&[0x10][..], &(-1i32).to_be_bytes()].concat()).as_i32(),
            Some(-1)
        );
        assert_eq!(
            parse(&[&[0x11][..], &i64::MIN.to_be_bytes()].concat()).as_i64(),
            Some(i64::MIN)
        );
        assert_eq!(
            parse(&[&[0x12][..], &(-42i128).to_be_bytes()].concat()).as_i128(),
            Some(-42)
        );

        assert_eq!(
            parse(&[&[0x14][..], &(-5i32).to_be_bytes()].concat()).as_dec32_unscaled(),
            Some(-5)
        );

        assert_eq!(
            parse(&[&[0x18][..], &1.5f32.to_be_bytes()].concat()).as_f32(),
            Some(1.5)
        );
        assert_eq!(
            parse(&[&[0x19][..], &(-0.25f64).to_be_bytes()].concat()).as_f64(),
            Some(-0.25)
        );

        assert_eq!(
            parse(&[&[0x1C][..], &20_000i32.to_be_bytes()].concat()).as_date32(),
            Some(20_000)
        );
        assert_eq!(
            parse(&[&[0x1D][..], &(-1i64).to_be_bytes()].concat()).as_timestamp64(),
            Some(-1)
        );
    }

    /// An accessor answers only for its own type. Same eight bytes under `u64`,
    /// `i64`, `dec64`, `f64`, `bytes8` and `timestamp64` — each reads out under
    /// exactly one of them, so no cell is ever silently reinterpreted.
    #[test]
    fn accessors_are_exclusive() {
        let payload = [0x80u8, 0, 0, 0, 0, 0, 0, 1];
        for id in [0x09u8, 0x0D, 0x11, 0x15, 0x19, 0x1D] {
            let bytes = [&[id][..], &payload].concat();
            let cell = CellValue::parse(&bytes).unwrap();

            let hits = [
                cell.as_bytes8().is_some(),
                cell.as_u64().is_some(),
                cell.as_i64().is_some(),
                cell.as_dec64_unscaled().is_some(),
                cell.as_f64().is_some(),
                cell.as_timestamp64().is_some(),
            ]
            .iter()
            .filter(|hit| **hit)
            .count();
            assert_eq!(hits, 1, "id {id:#04x} answered {hits} accessors");

            // Nor do the wrong-width or wrong-family accessors answer.
            assert_eq!(cell.as_u32(), None, "id {id:#04x}");
            assert_eq!(cell.as_u128(), None, "id {id:#04x}");
            assert_eq!(cell.as_bool(), None, "id {id:#04x}");
            assert_eq!(cell.as_str(), None, "id {id:#04x}");
            assert_eq!(cell.as_bytes(), None, "id {id:#04x}");
        }
    }

    /// The message quotes the offending bytes as hex, positioned, and says so
    /// when the value runs past the window.
    #[test]
    fn invalid_utf8_message_shows_the_bytes() {
        let msg = |cell: &[u8]| CellValue::parse(cell).unwrap_err().to_string();

        assert_eq!(
            msg(&[0x02, 3, 0xED, 0xA0, 0x80]),
            "str is not valid UTF-8 at byte 0 of 3: ed a0 80"
        );
        // The window starts at the failure, not at the value's start.
        assert_eq!(
            msg(&[0x02, 4, b'h', b'i', 0xC0, 0xAF]),
            "str is not valid UTF-8 at byte 2 of 4: c0 af"
        );

        // A value longer than the window is cut, and marked as cut.
        let mut long = vec![0x02, 40];
        long.extend_from_slice(&[b'a'; 20]);
        long.extend_from_slice(&[0xFF; 20]);
        assert_eq!(
            msg(&long),
            "str is not valid UTF-8 at byte 20 of 40: ff ff ff ff ff ff ff ff …"
        );

        // Exactly the window's worth, with nothing after it, is not marked.
        let exact = [&[0x02, 8][..], &[0xFF; 8]].concat();
        assert_eq!(
            msg(&exact),
            "str is not valid UTF-8 at byte 0 of 8: ff ff ff ff ff ff ff ff"
        );
    }

    /// `bytes` accepts every byte string `str` rejects — the UTF-8 rule is the
    /// type's, not the format's.
    #[test]
    fn bytes_accepts_what_str_rejects() {
        for (name, bytes, expected) in BAD_VECTORS {
            if !matches!(expected, CellParseError::InvalidUtf8 { .. }) {
                continue;
            }
            // Same length byte and value, only the type id swapped.
            // `parse_prefix`, since one vector carries a deliberate trailing
            // byte that is a framing matter rather than a content one.
            let as_bytes = [&[CellType::Bytes.id()][..], &bytes[1..]].concat();
            let (cell, _) =
                CellValue::parse_prefix(&as_bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cell.cell_type(), CellType::Bytes, "{name}");
            let declared = bytes[1] as usize;
            assert_eq!(cell.value(), &bytes[2..2 + declared], "{name}");
        }
    }

    /// A value too long to frame is rejected at construction — the wire form
    /// has no way to express it, so there is no cell to parse.
    #[test]
    fn over_long_values_cannot_be_built() {
        for ty in [CellType::Str, CellType::Bytes] {
            assert_eq!(
                CellValue::new(ty, &[b'a'; MAX_VALUE_LEN + 1], false).unwrap_err(),
                CellParseError::TooLong {
                    max: MAX_VALUE_LEN,
                    actual: MAX_VALUE_LEN + 1
                },
                "{ty:?}"
            );
        }
        // Exactly at the maximum still works.
        assert!(CellValue::new(CellType::Bytes, &[0u8; MAX_VALUE_LEN], false).is_ok());
    }

    /// The point of the length byte: cells pack adjacently and a truncated cell
    /// is rejected rather than read as a shorter valid value.
    #[test]
    fn packed_cells_walk_and_truncation_is_caught() {
        let seven = 7u64.to_be_bytes();
        let cells = [
            CellValue::new(CellType::Str, b"hi", true).unwrap(),
            CellValue::new(CellType::Uint(Width::W8), &seven, false).unwrap(),
            CellValue::new(CellType::Bytes, &[0xDE, 0xAD], false).unwrap(),
            CellValue::new(CellType::Tombstone, &[], false).unwrap(),
        ];

        let mut packed = Vec::new();
        for cell in &cells {
            cell.encode_into(&mut packed);
        }

        let mut rest = &packed[..];
        for expected in &cells {
            let (cell, tail) = CellValue::parse_prefix(rest).unwrap();
            assert_eq!(cell, *expected);
            rest = tail;
        }
        assert!(rest.is_empty());

        // Cutting the run anywhere never yields the same first cell followed by
        // a clean walk — the truncation is always caught.
        for cut in 1..packed.len() {
            let mut rest = &packed[..cut];
            let walked = std::iter::from_fn(|| match CellValue::parse_prefix(rest) {
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
            CellValue::parse(&[0x02, 0x02, b'h', b'i'])
                .unwrap()
                .as_str(),
            Some("hi")
        );
        assert_eq!(CellValue::parse(&[0x01, 1]).unwrap().as_bool(), Some(true));
        assert_eq!(CellValue::parse(&[0x01, 0]).unwrap().as_bool(), Some(false));
        // Wrong type: no coercion, no panic.
        assert_eq!(CellValue::parse(&[0x01, 1]).unwrap().as_str(), None);
        assert_eq!(CellValue::parse(&[0x00]).unwrap().as_bool(), None);
    }

    #[cfg(feature = "custom_types")]
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

    /// With the feature off the custom block decodes to nothing, and says so
    /// with its own error rather than pretending the ids are reserved.
    #[cfg(not(feature = "custom_types"))]
    #[test]
    fn custom_ids_are_rejected_without_the_feature() {
        for id in CUSTOM_TYPE_ID_BASE..TYPE_ID_SPACE {
            assert_eq!(
                CellType::from_id(id),
                Err(CellParseError::CustomTypesDisabled(id)),
                "id {id}"
            );
        }
        // The core block is untouched by the feature.
        assert_eq!(CellType::from_id(63), Err(CellParseError::ReservedType(63)));
        assert!(CellType::from_id(29).is_ok());
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
            if let Ok(cell) = CellValue::parse(&bytes) {
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
            let built = match CellValue::new(ty, &value, indexable) {
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
            let parsed = CellValue::parse(&encoded).map_err(|e| TestCaseError::fail(e.to_string()))?;
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
            let cell = CellValue::new(ty, &value, indexable).unwrap();
            let metadata = cell.metadata();
            prop_assert_eq!(metadata & TYPE_ID_MASK, ty.id());
            prop_assert_eq!(metadata & INDEXABLE_BIT != 0, indexable);
        }

        /// A `str` cell parses exactly when its bytes are UTF-8 — the parser
        /// agrees with the standard library, not with its own idea of UTF-8.
        #[test]
        fn str_accepts_exactly_utf8(value in prop::collection::vec(any::<u8>(), 0..=MAX_VALUE_LEN)) {
            let cell = [&[CellType::Str.id(), value.len() as u8][..], &value].concat();
            let parsed = CellValue::parse(&cell);
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
                .filter_map(|(ty, v)| CellValue::new(*ty, v, false).ok())
                .collect();

            let mut packed = Vec::new();
            for cell in &cells {
                cell.encode_into(&mut packed);
            }

            let mut rest = &packed[..];
            for expected in &cells {
                let (cell, tail) = CellValue::parse_prefix(rest)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                prop_assert_eq!(cell, *expected);
                rest = tail;
            }
            prop_assert!(rest.is_empty());
        }

        /// Reserved ids stay reserved whatever follows them. Scoped to the core
        /// block, since the custom block's verdict depends on the feature.
        #[test]
        fn reserved_ids_never_parse(
            id in (0u8..CUSTOM_TYPE_ID_BASE)
                .prop_filter("valid id", |id| CellType::from_id(*id).is_err()),
            indexable in any::<bool>(),
            tail in prop::collection::vec(any::<u8>(), 0..40),
        ) {
            let metadata = id | if indexable { INDEXABLE_BIT } else { 0 };
            let cell = [&[metadata][..], &tail].concat();
            prop_assert_eq!(CellValue::parse(&cell), Err(CellParseError::ReservedType(id)));
            prop_assert_eq!(CellValue::parse_prefix(&cell).err(), Some(CellParseError::ReservedType(id)));
        }

        /// The custom block never decodes to a core type, whatever follows it,
        /// and its verdict matches the feature this build was compiled with.
        #[test]
        fn custom_block_verdict_matches_the_feature(
            id in CUSTOM_TYPE_ID_BASE..TYPE_ID_SPACE,
            indexable in any::<bool>(),
            tail in prop::collection::vec(any::<u8>(), 0..40),
        ) {
            let metadata = id | if indexable { INDEXABLE_BIT } else { 0 };
            let cell = [&[metadata][..], &tail].concat();

            #[cfg(not(feature = "custom_types"))]
            prop_assert_eq!(
                CellValue::parse_prefix(&cell).err(),
                Some(CellParseError::CustomTypesDisabled(id))
            );

            // With the feature on the id always decodes; whether the cell as a
            // whole parses is a framing question, so anything that does parse
            // must come back as that custom type.
            #[cfg(feature = "custom_types")]
            {
                let ty = CellType::Custom(CustomTypeId::new(id).unwrap());
                prop_assert_eq!(CellType::from_id(id).unwrap(), ty);
                if let Ok((parsed, _)) = CellValue::parse_prefix(&cell) {
                    prop_assert_eq!(parsed.cell_type(), ty);
                }
            }
        }
    }
}
