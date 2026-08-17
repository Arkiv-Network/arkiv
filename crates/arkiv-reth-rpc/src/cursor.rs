//! Opaque pagination cursors for `arkiv_query`.
//!
//! A cursor is base64 of two little things: the entity id to resume below, and a
//! **binding** — a hash of the query text, the block the page was evaluated
//! against, and the projection. Resuming with a cursor whose binding doesn't
//! match the current request is an error rather than a silently different page.
//!
//! That matters because the underlying position is just an entity id. Handed a
//! bare id, a caller could pair page 2 of one query with page 1 of another and
//! get a result set that never existed at any single moment. Binding makes that
//! combination fail loudly, and keeps the encoding opaque so nobody builds a
//! client that arithmetics on it.

use alloy_primitives::keccak256;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The scheme marker the spec shows on a cursor.
const PREFIX: &str = "b64:";

/// Bytes of the binding hash kept. Eight is plenty to catch a mismatched
/// cursor; this is a consistency check, not a security boundary.
const BINDING_LEN: usize = 8;

/// What a cursor is tied to: this query, at this block, with this projection.
pub type Binding = [u8; BINDING_LEN];

/// Why a cursor was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorError {
    /// Not a cursor this node issued: wrong prefix, bad base64, wrong length.
    Malformed,
    /// A well-formed cursor from a *different* query, block or projection.
    Mismatched,
}

impl CursorError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Malformed => "cursor is malformed — pass back the cursor from the previous page",
            Self::Mismatched => {
                "cursor belongs to a different query, block or select — start a new page-through"
            }
        }
    }
}

/// The binding for a request. Length-prefixing the query keeps the parts from
/// running together, so two different requests cannot hash the same.
pub fn binding(query: &str, block: u64, projection_fingerprint: &[u8]) -> Binding {
    let mut preimage = Vec::with_capacity(query.len() + projection_fingerprint.len() + 24);
    preimage.extend_from_slice(b"arkiv.cursor");
    preimage.extend_from_slice(&(query.len() as u64).to_be_bytes());
    preimage.extend_from_slice(query.as_bytes());
    preimage.extend_from_slice(&block.to_be_bytes());
    preimage.extend_from_slice(projection_fingerprint);

    let digest = keccak256(preimage);
    let mut out = [0u8; BINDING_LEN];
    out.copy_from_slice(&digest[..BINDING_LEN]);
    out
}

/// Encode a resume position for a request with this binding.
pub fn encode(entity_id: u64, binding: Binding) -> String {
    let mut raw = [0u8; BINDING_LEN + size_of::<u64>()];
    raw[..BINDING_LEN].copy_from_slice(&binding);
    raw[BINDING_LEN..].copy_from_slice(&entity_id.to_be_bytes());
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
}

/// Decode a cursor, checking it was issued for this same request.
pub fn decode(text: &str, expected: Binding) -> Result<u64, CursorError> {
    let encoded = text.strip_prefix(PREFIX).ok_or(CursorError::Malformed)?;
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| CursorError::Malformed)?;
    if raw.len() != BINDING_LEN + size_of::<u64>() {
        return Err(CursorError::Malformed);
    }
    if raw[..BINDING_LEN] != expected {
        return Err(CursorError::Mismatched);
    }
    let id_bytes: [u8; 8] = raw[BINDING_LEN..].try_into().expect("checked length");
    Ok(u64::from_be_bytes(id_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINGERPRINT: &[u8] = b"select";

    fn bound() -> Binding {
        binding("a = true", 7, FINGERPRINT)
    }

    #[test]
    fn a_cursor_round_trips_within_its_own_request() {
        let cursor = encode(42, bound());
        assert_eq!(decode(&cursor, bound()).unwrap(), 42);
        // Including the edges.
        assert_eq!(decode(&encode(0, bound()), bound()).unwrap(), 0);
        assert_eq!(
            decode(&encode(u64::MAX, bound()), bound()).unwrap(),
            u64::MAX
        );
    }

    #[test]
    fn cursors_are_opaque_and_carry_the_scheme_marker() {
        let cursor = encode(42, bound());
        assert!(cursor.starts_with("b64:"), "{cursor}");
        // The id is not readable in the open, so nobody can do arithmetic on it.
        assert!(!cursor.contains("42"), "{cursor}");
    }

    #[test]
    fn a_cursor_from_another_request_is_refused() {
        let cursor = encode(42, bound());
        for other in [
            binding("a = false", 7, FINGERPRINT),    // different query
            binding("a = true", 8, FINGERPRINT),     // different block
            binding("a = true", 7, b"other select"), // different projection
        ] {
            assert_eq!(decode(&cursor, other), Err(CursorError::Mismatched));
        }
    }

    #[test]
    fn malformed_cursors_are_told_apart_from_mismatched_ones() {
        assert_eq!(decode("", bound()), Err(CursorError::Malformed));
        assert_eq!(decode("0x2a", bound()), Err(CursorError::Malformed));
        // The old transparent-hex form is no longer a cursor.
        assert_eq!(decode("b64:!!!!", bound()), Err(CursorError::Malformed));
        assert_eq!(
            decode(
                &format!("b64:{}", URL_SAFE_NO_PAD.encode([0u8; 4])),
                bound()
            ),
            Err(CursorError::Malformed),
        );
    }

    #[test]
    fn the_binding_separates_query_from_block_and_projection() {
        // Length-prefixing means a query that "borrows" the next field's bytes
        // still hashes differently.
        assert_ne!(binding("ab", 0, b""), binding("a", 0, b"b"));
    }
}
