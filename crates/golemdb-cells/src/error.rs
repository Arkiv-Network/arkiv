//! [`CellParseError`] — why a cell failed to parse.

use core::fmt;

use crate::UTF8_SNIPPET_LEN;
use crate::types::CellType;
#[cfg(doc)]
use crate::value::CellValue;

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
    /// [`CellValue::parse_prefix`] to walk a run of packed cells.
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
