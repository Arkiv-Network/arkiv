//! How one reth table row becomes one GolemDB record.
//!
//! reth addresses rows as `(table, key)`, or `(table, key, subkey)` for its four
//! DUPSORT tables. A record has a fixed 32-byte key and named cells, so:
//!
//! | cell | type     | holds                                    |
//! |------|----------|------------------------------------------|
//! | `t`  | `U64`    | the table's id                           |
//! | `o`  | `U256`   | the row key, left-padded to 32 bytes     |
//! | `s`  | `U256`   | the subkey, left-padded (DUPSORT only)   |
//! | `v`  | `BYTES`  | the compressed value                     |
//!
//! and the record key is `keccak(table ‖ key ‖ subkey)`, which is unique but says
//! nothing about order — the `o` cell is what cursors scan.
//!
//! # Why `U256` and not a string
//!
//! Cursors need `seek`: "the first row at or after this key". That is a range
//! predicate, and [`IndexClass::supports_range`] is true for only four core types —
//! `I32`, `U256`, `DEC`, `U64`. `STR` is prefix-only, so the obvious "hex-encode the
//! key" trick cannot answer `>=`. A big-endian `U256` compares identically to the raw
//! bytes it was padded from, so ordering by `o` is ordering by key.
//!
//! [`IndexClass::supports_range`]: arkiv_interfaces::store::IndexClass::supports_range
//!
//! # The 32-byte ceiling
//!
//! Padding into a `U256` caps a row key at 32 bytes. Every table this node writes is
//! within it — the widest are `B256` keys at exactly 32, and `StorageChangeSets`'
//! `BlockNumberAddress` at 28. The two that would overflow, `AccountsTrie` and
//! `StoragesTrie`, hold variable-length nibble paths and are never written here: the
//! state root is the store's `branch_digest`, so there is no trie to persist.
//!
//! [`encode_ordering`] returns `None` rather than truncating, so if a trie table ever
//! is written the failure is loud at the call site instead of silent corruption.

use alloy_primitives::keccak256;
use arkiv_interfaces::store::{Cell, CellName, RecordKey, TypeId};

/// Table id.
pub const CELL_TABLE: &str = "t";
/// Row key, left-padded. The cell cursors range-scan and sort on.
pub const CELL_ORDER: &str = "o";
/// DUPSORT subkey, left-padded. Absent on non-DUPSORT tables.
pub const CELL_SUBKEY: &str = "s";
/// The row's compressed value.
pub const CELL_VALUE: &str = "v";

/// The widest row key that fits the `U256` ordering cell.
pub const MAX_KEY_LEN: usize = 32;

/// Left-pad `bytes` into 32 big-endian bytes, preserving byte order.
///
/// `None` if `bytes` is wider than [`MAX_KEY_LEN`] — see the module docs.
pub fn encode_ordering(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() > MAX_KEY_LEN {
        return None;
    }
    let mut padded = [0u8; 32];
    padded[MAX_KEY_LEN - bytes.len()..].copy_from_slice(bytes);
    Some(padded)
}

/// The record key for a row: unique per `(table, key, subkey)`, order-free.
///
/// The preimage is **fixed width** — the table id, then the padded key, then a
/// presence byte, then the padded subkey. Concatenating the raw variable-width bytes
/// instead would be ambiguous: `("ab", "c")` and `("a", "bc")` would hash to the same
/// record, as would a subkey that is absent versus one that is empty. Padding removes
/// both cases by construction.
///
/// `None` if the key or subkey exceeds [`MAX_KEY_LEN`].
pub fn record_key(table: u64, key: &[u8], subkey: Option<&[u8]>) -> Option<RecordKey> {
    let mut preimage = [0u8; 8 + 32 + 1 + 32];
    preimage[..8].copy_from_slice(&table.to_be_bytes());
    preimage[8..40].copy_from_slice(&encode_ordering(key)?);
    if let Some(subkey) = subkey {
        preimage[40] = 1;
        preimage[41..].copy_from_slice(&encode_ordering(subkey)?);
    }
    Some(RecordKey::from_entity(keccak256(preimage).into()))
}

/// The cells of a row. `None` if the key or subkey exceeds [`MAX_KEY_LEN`].
pub fn cells(
    table: u64,
    key: &[u8],
    subkey: Option<&[u8]>,
    value: Vec<u8>,
) -> Option<Vec<(CellName, Cell)>> {
    let mut cells = vec![
        (
            CELL_TABLE.to_owned(),
            Cell::attribute(TypeId::U64, table.to_be_bytes().to_vec()),
        ),
        (
            CELL_ORDER.to_owned(),
            Cell::attribute(TypeId::U256, encode_ordering(key)?.to_vec()),
        ),
        (CELL_VALUE.to_owned(), Cell::field(TypeId::BYTES, value)),
    ];
    if let Some(subkey) = subkey {
        cells.push((
            CELL_SUBKEY.to_owned(),
            Cell::attribute(TypeId::U256, encode_ordering(subkey)?.to_vec()),
        ));
    }
    Some(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_preserves_byte_order() {
        // The whole reason for `U256`: comparing padded keys as big-endian integers
        // must give the same answer as comparing the raw keys as byte strings, or
        // `seek` returns the wrong row.
        let mut raw: Vec<Vec<u8>> = vec![
            vec![0x00],
            vec![0x01],
            vec![0x01, 0x00],
            vec![0x02],
            vec![0xff],
            vec![0xff, 0xff],
        ];
        let mut padded: Vec<[u8; 32]> = raw.iter().map(|k| encode_ordering(k).unwrap()).collect();
        padded.sort_unstable();
        raw.sort_by(|a, b| {
            // Byte-string order on equal-width-padded values is numeric order.
            encode_ordering(a)
                .unwrap()
                .cmp(&encode_ordering(b).unwrap())
        });
        let expected: Vec<[u8; 32]> = raw.iter().map(|k| encode_ordering(k).unwrap()).collect();
        assert_eq!(padded, expected);
    }

    #[test]
    fn a_u64_key_orders_numerically() {
        // `Headers`, `Transactions` and friends are keyed by block/tx number, and a
        // cursor walking them must see 9 before 10.
        let nine = encode_ordering(&9u64.to_be_bytes()).unwrap();
        let ten = encode_ordering(&10u64.to_be_bytes()).unwrap();
        assert!(nine < ten);
    }

    #[test]
    fn the_key_ceiling_is_reported_not_truncated() {
        assert!(encode_ordering(&[0u8; 32]).is_some());
        assert_eq!(encode_ordering(&[0u8; 33]), None);
        assert!(cells(1, &[0u8; 33], None, vec![]).is_none());
        assert!(cells(1, &[0u8; 1], Some(&[0u8; 33]), vec![]).is_none());
        assert!(record_key(1, &[0u8; 33], None).is_none());
        assert!(record_key(1, &[0u8; 1], Some(&[0u8; 33])).is_none());
    }

    #[test]
    fn record_keys_separate_every_dimension() {
        let k = |t, key: &[u8], sub: Option<&[u8]>| record_key(t, key, sub).unwrap();
        assert_ne!(k(1, b"a", None), k(2, b"a", None), "different table");
        assert_ne!(k(1, b"a", None), k(1, b"b", None), "different key");
        assert_ne!(
            k(1, b"a", Some(b"x")),
            k(1, b"a", Some(b"y")),
            "different subkey"
        );
        assert_ne!(
            k(1, b"a", None),
            k(1, b"a", Some(b"")),
            "subkey absent vs empty"
        );
        assert_eq!(
            k(1, b"a", Some(b"x")),
            k(1, b"a", Some(b"x")),
            "deterministic"
        );
    }

    #[test]
    fn the_preimage_split_is_unambiguous() {
        // Caught a real bug: hashing the raw bytes concatenated made these equal.
        assert_ne!(
            record_key(1, b"ab", Some(b"c")),
            record_key(1, b"a", Some(b"bc")),
            "key/subkey boundary must not be guessable from the concatenation"
        );
        assert_ne!(
            record_key(1, b"a", None),
            record_key(1, b"a", Some(b"")),
            "an absent subkey is not an empty one"
        );
    }

    #[test]
    fn a_row_carries_a_subkey_cell_only_when_dupsort() {
        let plain = cells(1, b"k", None, b"v".to_vec()).unwrap();
        assert!(plain.iter().all(|(name, _)| name != CELL_SUBKEY));
        let dup = cells(1, b"k", Some(b"s"), b"v".to_vec()).unwrap();
        assert!(dup.iter().any(|(name, _)| name == CELL_SUBKEY));
    }
}
