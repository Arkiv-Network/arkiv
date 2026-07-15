//! Validation and string-conversion impls for the ABI-generated [`Mime128`] struct.
//!
//! [`Mime128`] stores a MIME type string as 4 × `bytes32` (left-aligned, null-padded).
//! Validates per RFC 2045: `type/subtype[; param=value]*`, lowercase only, max 128 bytes.
//!
//! These impl blocks mirror the validation rules in `contracts/types/Mime128.sol`.

use alloy_primitives::FixedBytes;
use eyre::{Result, bail};

use crate::Mime128;

/// Printable ASCII (0x20–0x7E) excluding uppercase A-Z (0x41–0x5A).
///   bits 32–64  (0x20–0x40): set — space through @
///   bits 91–126 (0x5B–0x7E): set — [ through ~ (uppercase gap 0x41–0x5A is absent)
/// Mirrors `LOWER_PRINTABLE_ASCII` in Mime128.sol.
const LOWER_PRINTABLE_ASCII: u128 = (((1u128 << 33) - 1) << 32) | (((1u128 << 36) - 1) << 91);

/// Valid MIME token characters (RFC 2045, lowercase only).
/// Token chars = printable ASCII minus tspecials and uppercase.
/// tspecials: SPACE " ( ) , / : ; < = > ? @ [ \ ]
/// Mirrors `MIME_TOKEN` in Mime128.sol.
const MIME_TOKEN: u128 = LOWER_PRINTABLE_ASCII
    & !((1 << 0x20) // space
        | (1 << 0x22) // "
        | (1 << 0x28) // (
        | (1 << 0x29) // )
        | (1 << 0x2C) // ,
        | (1 << 0x2F) // /
        | (1 << 0x3A) // :
        | (1 << 0x3B) // ;
        | (1 << 0x3C) // <
        | (1 << 0x3D) // =
        | (1 << 0x3E) // >
        | (1 << 0x3F) // ?
        | (1 << 0x40) // @
        | (1 << 0x5B) // [
        | (1 << 0x5C) // \
        | (1 << 0x5D)); // ]

const S_TYPE: u8 = 0;
const S_SUBTYPE: u8 = 1;
const S_OWS: u8 = 2;
const S_PNAME: u8 = 3;
const S_PVALUE: u8 = 4;

fn is_token(b: u8) -> bool {
    b < 128 && (MIME_TOKEN >> b) & 1 == 1
}

impl Mime128 {
    /// Encode and validate a MIME type string into a `Mime128`.
    ///
    /// Validates per RFC 2045 (mirrors `validateMime128` in Mime128.sol):
    /// `type/subtype[; param=value]*`, lowercase only, at most 128 bytes.
    pub fn encode(s: &str) -> Result<Self> {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            bail!("MIME type cannot be empty");
        }
        if bytes.len() > 128 {
            bail!("MIME type too long: {} bytes (max 128)", bytes.len());
        }
        validate_mime_bytes(bytes)?;
        let mut data = [FixedBytes::ZERO; 4];
        for (i, chunk) in bytes.chunks(32).enumerate() {
            let mut buf = [0u8; 32];
            buf[..chunk.len()].copy_from_slice(chunk);
            data[i] = FixedBytes::from(buf);
        }
        Ok(Self { data })
    }

    /// Decode a `Mime128` to its string representation, stripping null padding.
    pub fn as_str(&self) -> Result<String> {
        let mut bytes = Vec::with_capacity(128);
        for b32 in &self.data {
            bytes.extend_from_slice(b32.as_slice());
        }
        if let Some(end) = bytes.iter().position(|&b| b == 0) {
            bytes.truncate(end);
        }
        String::from_utf8(bytes).map_err(|e| eyre::eyre!("invalid UTF-8 in Mime128: {}", e))
    }

    /// Validate a `Mime128`'s raw bytes, returning `self` if valid.
    ///
    /// Mirrors `validateMime128` in Mime128.sol.
    pub fn validate(self) -> Result<Self> {
        let s = self.as_str()?;
        if s.is_empty() {
            bail!("MIME type cannot be empty");
        }
        validate_mime_bytes(s.as_bytes())?;
        Ok(self)
    }
}

impl TryFrom<&str> for Mime128 {
    type Error = eyre::Error;
    fn try_from(s: &str) -> Result<Self> {
        Self::encode(s)
    }
}

impl std::fmt::Display for Mime128 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.as_str() {
            Ok(s) => write!(f, "{}", s),
            Err(_) => write!(f, "<invalid Mime128>"),
        }
    }
}

/// Validate raw MIME bytes using the RFC 2045 state machine.
/// Mirrors `validateMime128` in Mime128.sol.
fn validate_mime_bytes(bytes: &[u8]) -> Result<()> {
    let mut state = S_TYPE;
    let mut seg_len: usize = 0;

    for (pos, &b) in bytes.iter().enumerate() {
        match state {
            S_TYPE => {
                if is_token(b) {
                    seg_len += 1;
                } else if b == b'/' && seg_len > 0 {
                    state = S_SUBTYPE;
                    seg_len = 0;
                } else {
                    bail!("MIME invalid byte at position {}: 0x{:02x}", pos, b);
                }
            }
            S_SUBTYPE => {
                if is_token(b) {
                    seg_len += 1;
                } else if b == b';' && seg_len > 0 {
                    state = S_OWS;
                    seg_len = 0;
                } else {
                    bail!("MIME invalid byte at position {}: 0x{:02x}", pos, b);
                }
            }
            S_OWS => {
                if b == b' ' {
                    // stay
                } else if is_token(b) {
                    state = S_PNAME;
                    seg_len = 1;
                } else {
                    bail!("MIME invalid byte at position {}: 0x{:02x}", pos, b);
                }
            }
            S_PNAME => {
                if is_token(b) {
                    seg_len += 1;
                } else if b == b'=' && seg_len > 0 {
                    state = S_PVALUE;
                    seg_len = 0;
                } else {
                    bail!("MIME invalid byte at position {}: 0x{:02x}", pos, b);
                }
            }
            S_PVALUE => {
                if is_token(b) {
                    seg_len += 1;
                } else if b == b';' && seg_len > 0 {
                    state = S_OWS;
                    seg_len = 0;
                } else {
                    bail!("MIME invalid byte at position {}: 0x{:02x}", pos, b);
                }
            }
            _ => unreachable!(),
        }
    }

    if (state == S_SUBTYPE || state == S_PVALUE) && seg_len > 0 {
        Ok(())
    } else {
        bail!("MIME type incomplete")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let m = Mime128::encode("application/json").unwrap();
        assert_eq!(m.as_str().unwrap(), "application/json");
    }

    #[test]
    fn with_params() {
        let m = Mime128::encode("text/plain; charset=utf-8").unwrap();
        assert_eq!(m.as_str().unwrap(), "text/plain; charset=utf-8");
    }

    #[test]
    fn rejects_empty() {
        assert!(Mime128::encode("").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        assert!(Mime128::encode("Application/JSON").is_err());
    }

    #[test]
    fn rejects_missing_subtype() {
        assert!(Mime128::encode("text").is_err());
    }

    #[test]
    fn rejects_incomplete_param() {
        assert!(Mime128::encode("text/plain; charset").is_err());
    }

    #[test]
    fn validate_rejects_invalid_bytes() {
        let mut buf = [0u8; 32];
        buf[0] = 0xFF; // invalid UTF-8
        let m = Mime128 {
            data: [
                FixedBytes::from(buf),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn validate_rejects_invalid_mime_structure() {
        let mut buf = [0u8; 32];
        buf[..4].copy_from_slice(b"text"); // missing slash
        let m = Mime128 {
            data: [
                FixedBytes::from(buf),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        };
        assert!(m.validate().is_err());
    }
}
