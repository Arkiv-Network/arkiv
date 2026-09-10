//! [`CellValue`] — a parsed cell, and the accessors that read its value out.

use crate::error::CellParseError;
#[cfg(feature = "custom_types")]
use crate::types::CustomTypeId;
use crate::types::{CellType, FloatWidth, ValueLayout, Width};
use crate::{INDEXABLE_BIT, TYPE_ID_MASK};

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
    /// packed cells use [`CellValue::parse_prefix`].
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
    /// [`CellValue::parse`]. Appending several in a row produces a run
    /// [`CellValue::parse_prefix`] can walk back.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(2 + self.value.len());
        out.push(self.metadata());
        if self.ty.layout().has_length_byte() {
            // `new`/`parse` cap the value at MAX_VALUE_LEN, so this fits.
            out.push(self.value.len() as u8);
        }
        out.extend_from_slice(self.value);
    }

    /// This cell's wire bytes. [`CellValue::encode_into`] avoids the allocation.
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
