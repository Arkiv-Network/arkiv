//! Typed-literal validation: the raw text inside `i32(…)` → a checked
//! [`AttributeValue`].
//!
//! The lexer captures a literal's body verbatim; every rule about what may be in
//! there lives here, so the spec's literal table has exactly one implementation:
//!
//! | tag | accepted |
//! |---|---|
//! | `i32` | sign + digits, within [−2³¹, 2³¹−1] |
//! | `u256` | decimal digits or `0x` hex, ≤ 2²⁵⁶−1 |
//! | `dec` | sign, digits, `.` + ≤ 18 fractional digits — no exponent |
//! | `str` | `'…'` with `''` escapes, ≤ 128 bytes of UTF-8 |
//! | `addr` | `0x` + 40 hex, all-lower / all-upper / valid EIP-55 |
//! | `key`, `bytes32` | `0x` + 64 hex |
//! | `bool` | nothing — `true` / `false` are written bare |
//!
//! Excess precision on a `dec` is an **error, never a rounding**: silently
//! dropping a digit would make a query quietly match the wrong rows.
//!
//! The 256-bit arithmetic is done by hand on big-endian bytes. It only ever needs
//! "multiply by a small factor and add a digit", which is a dozen lines and keeps
//! the crate's arithmetic dependency-free.

use alloc::string::String;

use arkiv_interfaces::entity::{AttributeValue, DECIMAL_SCALE};
use arkiv_interfaces::primitives::Address;

use crate::error::{ParseError, literal_err};
use crate::lexer::{TypeTag, scan_single_quoted};

/// The longest `str` value the language accepts, in bytes. Matches the width the
/// ABI and the index's string cascade carry.
pub const MAX_STR_BYTES: usize = 128;

/// Hex characters in an address and in a 32-byte value.
const ADDRESS_HEX_LEN: usize = 40;
const WORD_HEX_LEN: usize = 64;

/// Validate a typed literal's raw body against its tag.
pub(crate) fn parse_tagged(
    tag: TypeTag,
    body: &str,
    position: usize,
) -> Result<AttributeValue, ParseError> {
    match tag {
        TypeTag::I32 => parse_i32(body, position).map(AttributeValue::Int),
        TypeTag::U64 => parse_u64(body, position).map(AttributeValue::U64),
        TypeTag::U256 => parse_u256(body, position).map(AttributeValue::U256),
        TypeTag::Dec => parse_dec(body, position).map(AttributeValue::Decimal),
        TypeTag::Str => parse_str(body, position).map(AttributeValue::Str),
        TypeTag::Addr => parse_addr(body, position).map(AttributeValue::EthereumAddress),
        TypeTag::Key => parse_word_hex(body, position, "key").map(AttributeValue::EntityKey),
        TypeTag::Bytes32 => parse_word_hex(body, position, "bytes32").map(AttributeValue::Bytes32),
        TypeTag::Bool => literal_err(
            position,
            "bool takes no wrapper — write the literal true or false",
        ),
    }
}

/// `i32(n)` — an optionally signed decimal, range-checked.
fn parse_i32(body: &str, position: usize) -> Result<i32, ParseError> {
    let (negative, digits) = split_sign(body.trim());
    let magnitude = decimal_magnitude_u64(digits, position, "i32")?;
    // The negative range reaches one further than the positive one.
    let limit = if negative {
        i32::MAX as u64 + 1
    } else {
        i32::MAX as u64
    };
    if magnitude > limit {
        return literal_err(
            position,
            "value is out of range for i32 [-2147483648, 2147483647] — use u256(…) for larger numbers",
        );
    }
    if negative {
        // `-(i32::MIN as i64)` is representable, so this round-trips at the edge.
        Ok(-(magnitude as i64) as i32)
    } else {
        Ok(magnitude as i32)
    }
}

/// `u64(n)` — decimal digits, or `0x` hex, range-checked.
///
/// This is the tag the system block heights take (`$expiresAt`, `$createdAt`,
/// `$updatedAt`), so it is the one most likely to be reached for by hand.
fn parse_u64(body: &str, position: usize) -> Result<u64, ParseError> {
    let text = body.trim();
    if text.starts_with('-') || text.starts_with('+') {
        return literal_err(position, "u64 is unsigned — remove the sign, or use i32(…)");
    }
    let value = if let Some(hex) = strip_hex_prefix(text) {
        if hex.is_empty() || hex.len() > 16 {
            return literal_err(
                position,
                "u64 hex must be 1 to 16 digits — use u256(…) for larger numbers",
            );
        }
        check_hex_digits(hex, position, "u64")?;
        u64::from_str_radix(hex, 16).map_err(|_| too_large(position, "u64"))?
    } else {
        decimal_magnitude_u64(text, position, "u64")?
    };
    Ok(value)
}

/// `u256(n)` — decimal digits, or `0x` hex.
fn parse_u256(body: &str, position: usize) -> Result<[u8; 32], ParseError> {
    let text = body.trim();
    if text.starts_with('-') || text.starts_with('+') {
        return literal_err(
            position,
            "u256 is unsigned — remove the sign, or use i32(…)",
        );
    }
    if let Some(hex) = strip_hex_prefix(text) {
        return hex_to_word(hex, position, "u256");
    }
    let accumulated = accumulate_decimal(text, position, "u256")?;
    Ok(accumulated.0)
}

/// `dec(n)` — a fixed-point decimal scaled by [`DECIMAL_SCALE`] places.
fn parse_dec(body: &str, position: usize) -> Result<[u8; 32], ParseError> {
    let (negative, rest) = split_sign(body.trim());
    let (whole, fraction) = match rest.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (rest, ""),
    };
    if whole.is_empty() {
        return literal_err(
            position,
            "dec needs at least one digit before the decimal point",
        );
    }
    if rest.contains('.') && fraction.is_empty() {
        return literal_err(
            position,
            "dec needs at least one digit after the decimal point",
        );
    }
    if fraction.len() > DECIMAL_SCALE as usize {
        return literal_err(
            position,
            "dec accepts at most 18 decimal places — excess precision is rejected, never rounded",
        );
    }

    let mut value = accumulate_decimal(whole, position, "dec")?;
    // Scale to 18 places, taking fraction digits where they exist and zeros after.
    let fraction_digits = fraction.as_bytes();
    for place in 0..DECIMAL_SCALE as usize {
        let digit = match fraction_digits.get(place) {
            Some(byte) => digit_value(*byte, position, "dec")?,
            None => 0,
        };
        value = value
            .mul_add(10, digit)
            .ok_or_else(|| too_large(position, "dec"))?;
    }

    // The scaled magnitude must fit a signed 256-bit word.
    if value.exceeds_i256_max() && !(negative && value.is_i256_min_magnitude()) {
        return Err(too_large(position, "dec"));
    }
    Ok(if negative { value.negate().0 } else { value.0 })
}

/// `str('…')` — a quoted string, length-checked.
fn parse_str(body: &str, position: usize) -> Result<String, ParseError> {
    let text = body.trim();
    if !text.starts_with('\'') {
        return literal_err(
            position,
            "str takes a single-quoted string, e.g. str('Bob')",
        );
    }
    let (content, end) = scan_single_quoted(text, 0)?;
    if end != text.len() {
        return literal_err(
            position,
            "unexpected text after the closing quote in str(…)",
        );
    }
    validate_str_len(&content, position)?;
    Ok(content)
}

/// Enforce the `str` length cap. Shared with bare `'…'` values.
pub(crate) fn validate_str_len(content: &str, position: usize) -> Result<(), ParseError> {
    if content.len() > MAX_STR_BYTES {
        return literal_err(position, "str values are limited to 128 bytes of UTF-8");
    }
    Ok(())
}

/// `addr(0x…)` — 40 hex, with EIP-55 enforced on mixed-case input.
pub(crate) fn parse_addr(body: &str, position: usize) -> Result<Address, ParseError> {
    let text = body.trim();
    let Some(hex) = strip_hex_prefix(text) else {
        return literal_err(
            position,
            "addr takes a 0x-prefixed address, e.g. addr(0xAbC…)",
        );
    };
    if hex.len() != ADDRESS_HEX_LEN {
        return literal_err(
            position,
            "an address is 0x followed by exactly 40 hex digits",
        );
    }
    check_hex_digits(hex, position, "addr")?;
    check_eip55(hex, position)?;

    let mut out = [0u8; 20];
    decode_hex(hex, &mut out);
    Ok(out)
}

/// `key(0x…)` / `bytes32(0x…)` — exactly 64 hex.
pub(crate) fn parse_word_hex(
    body: &str,
    position: usize,
    tag: &str,
) -> Result<[u8; 32], ParseError> {
    let text = body.trim();
    let Some(hex) = strip_hex_prefix(text) else {
        return literal_err(position, "expected a 0x-prefixed 32-byte value");
    };
    if hex.len() != WORD_HEX_LEN {
        return literal_err(position, "expected 0x followed by exactly 64 hex digits");
    }
    check_hex_digits(hex, position, tag)?;
    let mut out = [0u8; 32];
    decode_hex(hex, &mut out);
    Ok(out)
}

/// A bare decimal for a system attribute — a `u64` block height as a `u256`.
/// An untagged number literal — always `i32`, per the frozen grammar.
pub(crate) fn parse_bare_i32(text: &str, position: usize) -> Result<AttributeValue, ParseError> {
    parse_i32(text, position).map(AttributeValue::Int)
}

// ── shared helpers ──────────────────────────────────────────────────────

/// Split a leading sign off, returning whether it was negative.
fn split_sign(text: &str) -> (bool, &str) {
    if let Some(rest) = text.strip_prefix('-') {
        (true, rest)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    }
}

/// `0x`/`0X` prefix removal.
fn strip_hex_prefix(text: &str) -> Option<&str> {
    text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
}

/// One ASCII digit's value.
fn digit_value(byte: u8, position: usize, tag: &str) -> Result<u8, ParseError> {
    if byte.is_ascii_digit() {
        Ok(byte - b'0')
    } else {
        literal_err(position, &alloc::format!("{tag} expects decimal digits"))
    }
}

/// Parse decimal digits into a `u64`, erroring on overflow or a stray character.
fn decimal_magnitude_u64(digits: &str, position: usize, tag: &str) -> Result<u64, ParseError> {
    if digits.is_empty() {
        return literal_err(position, &alloc::format!("{tag} expects a number"));
    }
    let mut value: u64 = 0;
    for byte in digits.bytes() {
        let digit = digit_value(byte, position, tag)?;
        value = value
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit as u64))
            .ok_or_else(|| too_large(position, tag))?;
    }
    Ok(value)
}

/// Parse decimal digits into a 256-bit value.
fn accumulate_decimal(digits: &str, position: usize, tag: &str) -> Result<Uint256, ParseError> {
    if digits.is_empty() {
        return literal_err(position, &alloc::format!("{tag} expects a number"));
    }
    let mut value = Uint256::ZERO;
    for byte in digits.bytes() {
        let digit = digit_value(byte, position, tag)?;
        value = value
            .mul_add(10, digit)
            .ok_or_else(|| too_large(position, tag))?;
    }
    Ok(value)
}

/// Parse hex digits into a right-aligned 256-bit word.
fn hex_to_word(hex: &str, position: usize, tag: &str) -> Result<[u8; 32], ParseError> {
    check_hex_digits(hex, position, tag)?;
    let trimmed = hex.trim_start_matches('0');
    if trimmed.len() > WORD_HEX_LEN {
        return Err(too_large(position, tag));
    }
    let mut out = [0u8; 32];
    // Right-align: the last `trimmed.len()` nibbles of the word.
    let mut nibbles = trimmed.bytes().rev();
    let mut byte_index = out.len();
    while byte_index > 0 {
        let Some(low) = nibbles.next() else { break };
        let high = nibbles.next().unwrap_or(b'0');
        byte_index -= 1;
        out[byte_index] = (hex_nibble(high) << 4) | hex_nibble(low);
    }
    Ok(out)
}

/// Every character must be a hex digit.
fn check_hex_digits(hex: &str, position: usize, tag: &str) -> Result<(), ParseError> {
    if hex.is_empty() {
        return literal_err(
            position,
            &alloc::format!("{tag} expects hex digits after 0x"),
        );
    }
    if hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        literal_err(position, &alloc::format!("{tag} contains a non-hex digit"))
    }
}

/// Decode validated hex of exactly `out.len() * 2` characters.
fn decode_hex(hex: &str, out: &mut [u8]) {
    let bytes = hex.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = (hex_nibble(bytes[2 * index]) << 4) | hex_nibble(bytes[2 * index + 1]);
    }
}

/// One validated hex character's value.
fn hex_nibble(ch: u8) -> u8 {
    match ch {
        b'0'..=b'9' => ch - b'0',
        b'a'..=b'f' => ch - b'a' + 10,
        _ => ch - b'A' + 10,
    }
}

/// Reject a mixed-case address whose EIP-55 checksum doesn't hold.
///
/// All-lowercase and all-uppercase spellings carry no checksum, so they pass
/// unconditionally; any mix of cases is a claim about the hash and is verified.
fn check_eip55(hex: &str, position: usize) -> Result<(), ParseError> {
    let has_upper = hex.bytes().any(|byte| byte.is_ascii_uppercase());
    let has_lower = hex.bytes().any(|byte| byte.is_ascii_lowercase());
    if !(has_upper && has_lower) {
        return Ok(());
    }

    let mut lowercase = [0u8; ADDRESS_HEX_LEN];
    for (slot, byte) in lowercase.iter_mut().zip(hex.bytes()) {
        *slot = byte.to_ascii_lowercase();
    }
    let digest = alloy_primitives::keccak256(lowercase).0;

    for (index, byte) in hex.bytes().enumerate() {
        if byte.is_ascii_digit() {
            continue;
        }
        // Nibble `index` of the digest decides the case of character `index`.
        let nibble = if index % 2 == 0 {
            digest[index / 2] >> 4
        } else {
            digest[index / 2] & 0x0f
        };
        if byte.is_ascii_uppercase() != (nibble >= 8) {
            return literal_err(
                position,
                "address fails its EIP-55 checksum — fix the capitalization, or write it all-lowercase",
            );
        }
    }
    Ok(())
}

/// The "doesn't fit" error, phrased per tag.
fn too_large(position: usize, tag: &str) -> ParseError {
    ParseError::literal(position, alloc::format!("value is out of range for {tag}"))
}

// ── 256-bit arithmetic ──────────────────────────────────────────────────

/// A big-endian 256-bit unsigned value, built up one digit at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Uint256([u8; 32]);

impl Uint256 {
    const ZERO: Self = Self([0u8; 32]);

    /// `self * factor + addend`, or `None` if that leaves 256 bits.
    ///
    /// `factor` is 10 or 16 and `addend` is one digit, so the per-byte product
    /// stays well inside a `u16`.
    fn mul_add(self, factor: u8, addend: u8) -> Option<Self> {
        let mut out = self.0;
        let mut carry = u16::from(addend);
        for byte in out.iter_mut().rev() {
            let product = u16::from(*byte) * u16::from(factor) + carry;
            *byte = (product & 0xff) as u8;
            carry = product >> 8;
        }
        (carry == 0).then_some(Self(out))
    }

    /// Whether the top bit is set — the magnitude is ≥ 2²⁵⁵, so it cannot be a
    /// positive `i256`.
    fn exceeds_i256_max(&self) -> bool {
        self.0[0] & 0x80 != 0
    }

    /// Exactly 2²⁵⁵ — the one magnitude only the negative side can hold.
    fn is_i256_min_magnitude(&self) -> bool {
        self.0[0] == 0x80 && self.0[1..].iter().all(|byte| *byte == 0)
    }

    /// Two's-complement negation.
    fn negate(self) -> Self {
        let mut out = self.0;
        for byte in out.iter_mut() {
            *byte = !*byte;
        }
        for byte in out.iter_mut().rev() {
            let (sum, carried) = byte.overflowing_add(1);
            *byte = sum;
            if !carried {
                break;
            }
        }
        Self(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i32_of(body: &str) -> Result<i32, ParseError> {
        parse_i32(body, 0)
    }

    fn dec_of(body: &str) -> Result<[u8; 32], ParseError> {
        parse_dec(body, 0)
    }

    fn u256_of(body: &str) -> Result<[u8; 32], ParseError> {
        parse_u256(body, 0)
    }

    /// A `u64` as the right-aligned word the parsers produce.
    fn word(value: u128) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[16..].copy_from_slice(&value.to_be_bytes());
        out
    }

    #[test]
    fn i32_covers_its_whole_range() {
        assert_eq!(i32_of("0").unwrap(), 0);
        assert_eq!(i32_of("-1").unwrap(), -1);
        assert_eq!(i32_of("2147483647").unwrap(), i32::MAX);
        assert_eq!(i32_of("-2147483648").unwrap(), i32::MIN);
        assert_eq!(i32_of("+7").unwrap(), 7);
    }

    #[test]
    fn i32_rejects_out_of_range_and_junk() {
        assert!(i32_of("2147483648").is_err());
        assert!(i32_of("-2147483649").is_err());
        assert!(i32_of("").is_err());
        assert!(i32_of("12a").is_err());
        // The message points at the fix.
        let err = i32_of("99999999999").unwrap_err();
        assert!(err.message.contains("u256"), "{err}");
    }

    #[test]
    fn u256_takes_decimal_and_hex() {
        assert_eq!(u256_of("0").unwrap(), word(0));
        assert_eq!(u256_of("1000000").unwrap(), word(1_000_000));
        assert_eq!(u256_of("0xf4240").unwrap(), word(0xf_4240));
        assert_eq!(u256_of("0X01").unwrap(), word(1));
        // Leading zeros don't count against the width.
        let mut padded = String::from("0x");
        padded.push_str(&"0".repeat(100));
        padded.push('5');
        assert_eq!(u256_of(&padded).unwrap(), word(5));
    }

    #[test]
    fn u256_holds_its_maximum_and_rejects_more() {
        let max_hex = alloc::format!("0x{}", "f".repeat(64));
        assert_eq!(u256_of(&max_hex).unwrap(), [0xff; 32]);
        assert!(u256_of(&alloc::format!("0x{}", "f".repeat(65))).is_err());
        // 2^256 in decimal is one past the top.
        let two_pow_256 =
            "115792089237316195423570985008687907853269984665640564039457584007913129639936";
        assert!(u256_of(two_pow_256).is_err());
        // 2^256 - 1 fits exactly.
        let max_decimal =
            "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(u256_of(max_decimal).unwrap(), [0xff; 32]);
    }

    #[test]
    fn u256_rejects_signs() {
        assert!(u256_of("-1").is_err());
        assert!(u256_of("+1").is_err());
    }

    #[test]
    fn dec_scales_by_eighteen_places() {
        // 1.5 → 1_500_000_000_000_000_000
        assert_eq!(dec_of("1.5").unwrap(), word(1_500_000_000_000_000_000));
        // A bare integer is scaled the same way.
        assert_eq!(dec_of("3").unwrap(), word(3_000_000_000_000_000_000));
        assert_eq!(dec_of("0").unwrap(), word(0));
        // Trailing zeros in the fraction don't change the value.
        assert_eq!(dec_of("3.50").unwrap(), dec_of("3.5").unwrap());
        // Exactly 18 places is allowed.
        assert!(dec_of("0.123456789012345678").is_ok());
    }

    #[test]
    fn dec_negatives_are_twos_complement() {
        let minus_one = dec_of("-1").unwrap();
        let plus_one = dec_of("1").unwrap();

        // -x + x is zero in the full 256-bit word (the final carry falls off).
        let mut sum = [0u8; 32];
        let mut carry = 0u16;
        for index in (0..32).rev() {
            let total = u16::from(minus_one[index]) + u16::from(plus_one[index]) + carry;
            sum[index] = (total & 0xff) as u8;
            carry = total >> 8;
        }
        assert_eq!(sum, [0u8; 32], "negation should round-trip to zero");

        // The sign bit is set for negatives and clear for positives.
        assert_eq!(minus_one[0] & 0x80, 0x80);
        assert_eq!(plus_one[0] & 0x80, 0x00);
    }

    #[test]
    fn dec_rejects_excess_precision_rather_than_rounding() {
        let err = dec_of("0.1234567890123456789").unwrap_err();
        assert!(err.message.contains("never rounded"), "{err}");
    }

    #[test]
    fn dec_rejects_malformed_shapes() {
        assert!(dec_of(".5").is_err()); // no digit before the point
        assert!(dec_of("1.").is_err()); // no digit after the point
        assert!(dec_of("1e10").is_err()); // no exponents
        assert!(dec_of("").is_err());
        assert!(dec_of("1.2.3").is_err());
    }

    #[test]
    fn str_unquotes_and_bounds_length() {
        assert_eq!(parse_str("'Bob'", 0).unwrap(), "Bob");
        assert_eq!(parse_str("''", 0).unwrap(), "");
        assert_eq!(parse_str("'it''s'", 0).unwrap(), "it's");
        // Multibyte content is measured in bytes.
        let max = alloc::format!("'{}'", "a".repeat(MAX_STR_BYTES));
        assert!(parse_str(&max, 0).is_ok());
        let over = alloc::format!("'{}'", "a".repeat(MAX_STR_BYTES + 1));
        assert!(parse_str(&over, 0).is_err());
        // Unquoted input is rejected with a directive message.
        let err = parse_str("Bob", 0).unwrap_err();
        assert!(err.message.contains("str('Bob')"), "{err}");
    }

    #[test]
    fn addr_accepts_uniform_case() {
        let lower = alloc::format!("0x{}", "ab".repeat(20));
        assert_eq!(parse_addr(&lower, 0).unwrap(), [0xab; 20]);
        let upper = alloc::format!("0x{}", "AB".repeat(20));
        assert_eq!(parse_addr(&upper, 0).unwrap(), [0xab; 20]);
        // Digits only is both all-upper and all-lower.
        assert!(parse_addr(&alloc::format!("0x{}", "1".repeat(40)), 0).is_ok());
    }

    #[test]
    fn addr_checks_mixed_case_against_eip55() {
        // The canonical EIP-55 vectors.
        for checksummed in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ] {
            assert!(parse_addr(checksummed, 0).is_ok(), "{checksummed}");
        }
        // Swapping the case of one letter breaks the checksum.
        let err = parse_addr("0x5aAeb6053f3E94C9b9A09f33669435E7Ef1BeAed", 0).unwrap_err();
        assert!(err.message.contains("EIP-55"), "{err}");
    }

    #[test]
    fn addr_rejects_bad_shapes() {
        assert!(parse_addr("0xdead", 0).is_err()); // too short
        assert!(parse_addr(&alloc::format!("0x{}", "a".repeat(41)), 0).is_err());
        assert!(parse_addr(&"a".repeat(40), 0).is_err()); // missing 0x
        assert!(parse_addr(&alloc::format!("0x{}", "z".repeat(40)), 0).is_err());
    }

    #[test]
    fn key_and_bytes32_need_exactly_64_hex() {
        let hex = alloc::format!("0x{}", "cd".repeat(32));
        assert_eq!(parse_word_hex(&hex, 0, "key").unwrap(), [0xcd; 32]);
        assert!(parse_word_hex("0xcd", 0, "key").is_err());
        assert!(parse_word_hex(&alloc::format!("0x{}", "cd".repeat(33)), 0, "key").is_err());
        // Mixed case is fine here — only addresses carry a checksum.
        let mixed = alloc::format!("0x{}", "cD".repeat(32));
        assert!(parse_word_hex(&mixed, 0, "bytes32").is_ok());
    }

    #[test]
    fn bool_rejects_the_wrapper_form() {
        let err = parse_tagged(TypeTag::Bool, "true", 0).unwrap_err();
        assert!(err.message.contains("no wrapper"), "{err}");
    }

    /// An untagged number is an `i32` — the frozen grammar's default, and the
    /// only untagged numeric form.
    #[test]
    fn bare_numbers_are_i32() {
        assert_eq!(parse_bare_i32("10", 0).unwrap(), AttributeValue::Int(10));
        assert_eq!(parse_bare_i32("-10", 0).unwrap(), AttributeValue::Int(-10));
        // Out of i32 range: a bare number never silently widens.
        assert!(parse_bare_i32("2147483648", 0).is_err());
        assert!(parse_bare_i32("abc", 0).is_err());
    }

    /// `u64(…)` takes decimal or hex and is range-checked at both ends.
    #[test]
    fn u64_accepts_decimal_and_hex_within_range() {
        let parse = |body: &str| parse_tagged(TypeTag::U64, body, 0);
        assert_eq!(parse("0").unwrap(), AttributeValue::U64(0));
        assert_eq!(parse("1200000").unwrap(), AttributeValue::U64(1_200_000));
        assert_eq!(parse("0x1a").unwrap(), AttributeValue::U64(0x1a));
        assert_eq!(
            parse("18446744073709551615").unwrap(),
            AttributeValue::U64(u64::MAX)
        );
        assert_eq!(
            parse("0xffffffffffffffff").unwrap(),
            AttributeValue::U64(u64::MAX)
        );
        // Past the top, signed, and over-wide hex all fail rather than wrap.
        assert!(parse("18446744073709551616").is_err());
        assert!(parse("-1").is_err());
        assert!(parse("0x10000000000000000").is_err());
        assert!(parse("0x").is_err());
        assert!(parse("nope").is_err());
    }

    #[test]
    fn mul_add_detects_overflow_at_the_top() {
        let max = Uint256([0xff; 32]);
        assert!(max.mul_add(10, 0).is_none());
        assert!(max.mul_add(1, 1).is_none());
        assert_eq!(Uint256::ZERO.mul_add(10, 7).unwrap().0[31], 7);
    }
}
