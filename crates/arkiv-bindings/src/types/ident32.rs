//! Validation and string-conversion impls for the ABI-generated [`Ident32`] UDVT.
//!
//! [`Ident32`] is a left-aligned, null-padded 32-byte ASCII identifier.
//! Valid characters: `A-Z a-z`, `0-9`, `.`, `-`, `_`. Must start with `A-Z a-z`.
//!
//! Names follow the attribute-name grammar in `arkiv-node-api.md`.

use alloy_primitives::FixedBytes;
use arkiv_interfaces::entity::annotations::SYSTEM_PREFIX;
use eyre::{Result, bail};

use crate::Ident32;

/// Valid character bitmap: A-Z, a-z, 0-9, '.', '-', '_'.
const IDENT_CHARSET: u128 = (1 << 0x2D)
    | (1 << 0x2E)
    | (((1 << 10) - 1) << 0x30)
    | (((1u128 << 26) - 1) << 0x41)
    | (1 << 0x5F)
    | (((1u128 << 26) - 1) << 0x61);

/// Leading byte bitmap: A-Z and a-z only.
const IDENT_LEADING: u128 = (((1u128 << 26) - 1) << 0x41) | (((1u128 << 26) - 1) << 0x61);

/// A structured Ident32 validation failure, carrying the evidence the
/// `Ident32Empty` / `Ident32InvalidByte` ABI errors report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ident32ByteError {
    /// The name starts with a null byte — an empty identifier.
    Empty,
    /// A byte is outside its position's charset, or non-null after the
    /// null padding began.
    InvalidByte { position: usize, value: u8 },
}

/// Validate raw bytes as an `Ident32`, reporting the first offending byte.
///
/// The name must start with an ASCII letter. Other bytes must be ASCII
/// letters, digits, `.`, `-`, or `_`. All bytes after the first null must be null.
pub fn validate_ident32_bytes(bytes: &[u8; 32]) -> Result<(), Ident32ByteError> {
    if bytes[0] == 0 {
        return Err(Ident32ByteError::Empty);
    }
    let mut seen_zero = false;
    for (position, &b) in bytes.iter().enumerate() {
        if b == 0 {
            seen_zero = true;
        } else {
            let charset = if position == 0 {
                IDENT_LEADING
            } else {
                IDENT_CHARSET
            };
            let charset_bad = b > 127 || (charset >> b) & 1 == 0;
            if seen_zero || charset_bad {
                return Err(Ident32ByteError::InvalidByte { position, value: b });
            }
        }
    }
    Ok(())
}

/// Validate a **system** attribute name (`$payload`, `$contentType`).
///
/// System names use a separate namespace with a leading `$`.
/// The caller checks writable names against `annotations::USER_MANAGED`.
///
/// So this enforces only what the encoding itself needs: a leading `$`, a
/// non-empty remainder, and contiguous null padding (no bytes after the
/// padding starts, which would make two spellings of one name).
pub fn validate_system_ident32_bytes(bytes: &[u8; 32]) -> Result<(), Ident32ByteError> {
    if bytes[0] != SYSTEM_PREFIX {
        return Err(Ident32ByteError::InvalidByte {
            position: 0,
            value: bytes[0],
        });
    }
    if bytes[1] == 0 {
        // `$` alone is not a name.
        return Err(Ident32ByteError::Empty);
    }
    let mut seen_zero = false;
    for (position, &b) in bytes.iter().enumerate().skip(1) {
        if b == 0 {
            seen_zero = true;
        } else if seen_zero {
            return Err(Ident32ByteError::InvalidByte { position, value: b });
        }
    }
    Ok(())
}

impl Ident32 {
    /// The underlying 32-byte word.
    ///
    /// `sol!` represents a UDVT as its underlying type wherever it appears in a
    /// struct or call, so this is what callers outside this crate need to hand
    /// an `Ident32` to a generated type. The wrapper's own field is private to
    /// this crate.
    pub fn into_word(self) -> FixedBytes<32> {
        self.0
    }

    /// Wrap a raw word, without validating it — for reading names *back* out of
    /// ABI data that the node already accepted.
    pub fn from_word(word: FixedBytes<32>) -> Self {
        Self(word)
    }

    /// Encode a **system** attribute name (leading `$`), for callers building
    /// `$payload` / `$contentType` triples. [`Ident32::encode`] rejects these
    /// by design — its leading-byte charset is `A-Z a-z`.
    pub fn system(s: &str) -> Result<Self> {
        let bytes = s.as_bytes();
        if bytes.len() > 32 {
            eyre::bail!("system ident too long: {} bytes (max 32)", bytes.len());
        }
        let mut word = [0u8; 32];
        word[..bytes.len()].copy_from_slice(bytes);
        validate_system_ident32_bytes(&word)
            .map_err(|e| eyre::eyre!("invalid system ident '{s}': {e:?}"))?;
        Ok(Self(alloy_primitives::FixedBytes::from(word)))
    }

    /// Encode a string into an `Ident32`, validating the charset.
    ///
    /// Attribute-name rules:
    /// - Non-empty, at most 32 bytes
    /// - First byte must be `A-Z a-z`
    /// - Remaining bytes must be in `A-Z a-z 0-9 . - _`
    pub fn encode(s: &str) -> Result<Self> {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            bail!("Ident32 cannot be empty");
        }
        if bytes.len() > 32 {
            bail!("Ident32 too long: {} bytes (max 32)", bytes.len());
        }
        if (IDENT_LEADING >> bytes[0]) & 1 == 0 {
            bail!(
                "Ident32 invalid leading byte at position 0: 0x{:02x}",
                bytes[0]
            );
        }
        for (i, &b) in bytes.iter().enumerate().skip(1) {
            if (IDENT_CHARSET >> b) & 1 == 0 {
                bail!("Ident32 invalid byte at position {}: 0x{:02x}", i, b);
            }
        }
        let mut buf = [0u8; 32];
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(Self(FixedBytes::from(buf)))
    }

    /// Decode an `Ident32` to its string representation, stripping null padding.
    pub fn decode(&self) -> Result<String> {
        let bytes = self.0.as_slice();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(32);
        String::from_utf8(bytes[..end].to_vec())
            .map_err(|e| eyre::eyre!("invalid UTF-8 in Ident32: {}", e))
    }

    /// Validate raw bytes as an `Ident32`, returning `self` if valid.
    ///
    /// In addition to charset validation, enforces that once a null byte
    /// appears all subsequent bytes must also be null (no embedded nulls).
    /// This matches the stricter check in `validateIdent32` in Ident32.sol.
    pub fn validate(self) -> Result<Self> {
        let bytes = self.0.as_slice();
        if bytes[0] == 0 {
            bail!("Ident32 cannot be empty");
        }
        if (IDENT_LEADING >> bytes[0]) & 1 == 0 {
            bail!(
                "Ident32 invalid leading byte at position 0: 0x{:02x}",
                bytes[0]
            );
        }
        let mut found_null = false;
        for (i, &b) in bytes.iter().enumerate().skip(1) {
            if found_null {
                if b != 0 {
                    bail!(
                        "Ident32 embedded null: non-zero byte 0x{:02x} at position {}",
                        b,
                        i
                    );
                }
            } else if b == 0 {
                found_null = true;
            } else if (IDENT_CHARSET >> b) & 1 == 0 {
                bail!("Ident32 invalid byte at position {}: 0x{:02x}", i, b);
            }
        }
        Ok(self)
    }
}

impl TryFrom<&str> for Ident32 {
    type Error = eyre::Error;
    fn try_from(s: &str) -> Result<Self> {
        Self::encode(s)
    }
}

impl std::fmt::Display for Ident32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.decode() {
            Ok(s) => write!(f, "{}", s),
            Err(_) => write!(f, "{}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let id = Ident32::encode("my.attribute").unwrap();
        assert_eq!(id.decode().unwrap(), "my.attribute");
    }

    #[test]
    fn rejects_empty() {
        assert!(Ident32::encode("").is_err());
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(33);
        assert!(Ident32::encode(&s).is_err());
    }

    #[test]
    fn accepts_uppercase_and_preserves_case() {
        for name in [
            "Hello",
            "projectId",
            "LEVEL",
            "a.b-c_D9",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345",
        ] {
            let id = Ident32::encode(name).unwrap();
            assert_eq!(id.decode().unwrap(), name);
            assert!(validate_ident32_bytes(&id.0.0).is_ok());
            assert_eq!(id.validate().unwrap().decode().unwrap(), name);
        }
        assert_ne!(
            Ident32::encode("Level").unwrap().into_word(),
            Ident32::encode("level").unwrap().into_word()
        );
    }

    #[test]
    fn rejects_leading_digit() {
        assert!(Ident32::encode("1foo").is_err());
    }

    #[test]
    fn accepts_valid_chars() {
        assert!(Ident32::encode("my-attr_name.v2").is_ok());
    }

    #[test]
    fn full_length_32_bytes() {
        let s = "a".repeat(32);
        let id = Ident32::encode(&s).unwrap();
        assert_eq!(id.decode().unwrap().len(), 32);
    }

    #[test]
    fn validate_rejects_embedded_null() {
        // TryFrom<FixedBytes<32>> is provided by alloy (non-validating From).
        // Use Ident32(raw).validate() to enforce the contract's stricter check.
        let mut buf = [0u8; 32];
        buf[0] = b'a';
        buf[1] = b'b';
        buf[2] = 0; // null terminator
        buf[3] = b'c'; // non-null after null — contract rejects this
        let raw = FixedBytes::<32>::from(buf);
        assert!(Ident32(raw).validate().is_err());
    }

    #[test]
    fn validate_accepts_null_terminated() {
        let mut buf = [0u8; 32];
        buf[0] = b'a';
        buf[1] = b'b';
        // remaining bytes are zero — valid
        let raw = FixedBytes::<32>::from(buf);
        assert!(Ident32(raw).validate().is_ok());
    }
}
