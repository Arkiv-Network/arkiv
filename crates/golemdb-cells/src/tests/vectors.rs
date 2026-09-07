//! The cell vectors the suite is written against: one table of byte strings
//! that must parse, one of byte strings that must not.
//!
//! Data only — the assertions over these live in [`super`].

use crate::*;

/// One test vector: name, wire bytes, indexable, type, value bytes.
pub(super) type Vector = (&'static str, &'static [u8], bool, CellType, &'static [u8]);

/// One cell per row: its wire bytes, and what they mean. Every core family
/// appears at least once, at both settings of the indexable bit.
#[rustfmt::skip]
pub(super) const VECTORS: &[Vector] = &[
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
pub(super) const CUSTOM_VECTORS: &[Vector] = &[
    ("custom 64, 0 B", &[0x40, 0x00],                    false, CellType::Custom(CustomTypeId::new(64).unwrap()), &[]),
    ("custom 100",     &[0x64, 0x03, 9, 9, 9],           false, CellType::Custom(CustomTypeId::new(100).unwrap()), &[9, 9, 9]),
    ("custom 127, idx",&[0xFF, 0x01, 1],                 true,  CellType::Custom(CustomTypeId::new(127).unwrap()), &[1]),
];

#[cfg(not(feature = "custom_types"))]
const CUSTOM_VECTORS: &[Vector] = &[];

/// Every accept vector that applies to this build.
pub(super) fn accept_vectors() -> impl Iterator<Item = &'static Vector> {
    VECTORS.iter().chain(CUSTOM_VECTORS)
}

// The 16-, 20- and 32-byte vectors, lifted out of the table above so its
// rows stay one line each. Every one is an all-zero value of a fixed-width
// type, so there is no length byte and the value is the whole tail.
//
// `<TYPE>_ZERO` is the value; `<TYPE>_ZERO_CELL` is that value with its
// metadata byte in front.
pub(super) const ZERO_VALUE_16: [u8; 16] = [0; 16];
pub(super) const ZERO_VALUE_20: [u8; 20] = [0; 20];
pub(super) const ZERO_VALUE_32: [u8; 32] = [0; 32];

pub(super) const BYTES20_ZERO_CELL: [u8; 21] = zero_cell_20(CellType::Bytes20);

pub(super) const BYTES16_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::FixedBytes(Width::W16));
pub(super) const U128_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::Uint(Width::W16));
pub(super) const I128_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::Int(Width::W16));
pub(super) const DEC128_ZERO_CELL: [u8; 17] = zero_cell_16(CellType::Decimal(Width::W16));

pub(super) const BYTES32_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::FixedBytes(Width::W32));
pub(super) const U256_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::Uint(Width::W32));
pub(super) const I256_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::Int(Width::W32));
pub(super) const DEC256_ZERO_CELL: [u8; 33] = zero_cell_32(CellType::Decimal(Width::W32));

/// A whole cell of a 16-byte-wide type: `ty`'s metadata byte, then a
/// 16-byte zero value.
///
/// Taking a [`CellType`] rather than a raw byte is what keeps the constants
/// above readable — the type is named, not spelled as a hex id. One
/// function per width, because an array's length is part of its type and a
/// `const fn` cannot be generic over it here.
pub(super) const fn zero_cell_16(ty: CellType) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[0] = ty.id();
    out
}

/// A whole cell of a 20-byte-wide type. See [`zero_cell_16`].
pub(super) const fn zero_cell_20(ty: CellType) -> [u8; 21] {
    let mut out = [0u8; 21];
    out[0] = ty.id();
    out
}

/// A whole cell of a 32-byte-wide type. See [`zero_cell_16`].
pub(super) const fn zero_cell_32(ty: CellType) -> [u8; 33] {
    let mut out = [0u8; 33];
    out[0] = ty.id();
    out
}

/// One rejection vector: name, wire bytes, and the error they must produce.
///
/// The error is asserted exactly, not just "it failed" — a cell rejected
/// for the wrong reason is a bug the way a cell accepted wrongly is.
pub(super) type BadVector = (&'static str, &'static [u8], CellParseError);

/// [`CellParseError::LengthMismatch`] as a call rather than a struct
/// literal, so the rows below stay one line each and stay aligned.
pub(super) const fn length_mismatch(
    ty: CellType,
    expected: usize,
    actual: usize,
) -> CellParseError {
    CellParseError::LengthMismatch {
        ty,
        expected,
        actual,
    }
}

#[rustfmt::skip]
pub(super) const BAD_VECTORS: &[BadVector] = &[
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
