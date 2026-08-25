//! The int-mode range index: attribute values in sorted order, range-scannable.
//!
//! This is the caller-facing layer over the [`btree`](crate::indices::btree). It turns an
//! attribute *value* (≤ 32 bytes) into a B+ tree key and back, so a `$expiration >
//! N` style predicate can enumerate the matching values — each of which the query
//! layer then resolves to its tier-1 [`pair_address`](crate::indices::address::pair_address)
//! bitmap.
//!
//! Encoding, ported verbatim from `arkiv-db-engine` and **consensus-critical**:
//! - **Key** = the value right-padded to 32 bytes ([`annot_val_to_slot`]). Values
//!   carry no null bytes (the precompile enforces it), so the padding is
//!   unambiguous, and fixed-width big-endian values (block numbers, uints) sort
//!   numerically under the tree's byte ordering.
//! - **Value** = [`slot_presence`]: `len + 1` in the low 4 bytes. The `+ 1` is what
//!   lets `0` mean "absent" (a lazy-deleted or never-written slot) while still
//!   recording a genuinely empty value (`len = 0`).
//!
//! Only values ≤ 32 bytes belong here; longer string values use the cascade (a
//! later module).

use alloy_primitives::B256;
use arkiv_interfaces::constants::ethereum::EVM_WORD_LENGTH;
use arkiv_interfaces::entity::AttributeType;

use crate::indices::address::btree_header_address;
use crate::indices::btree;
use crate::indices::storage::IndexStorage;

/// Encode a value (≤ [`EVM_WORD_LENGTH`] bytes) as its B+ tree **key** by **left**-aligning
/// it and zero-padding the tail:
///
/// ```text
///   byte:  0 ....... value.len() ....... EVM_WORD_LENGTH
///          | value             | 0 (padding)     |
/// ```
///
/// Left alignment makes byte order match value order, so a fixed-width big-endian
/// value (a block number, a uint) sorts numerically in the tree. Values carry no
/// null bytes (the precompile enforces it), so the padding is unambiguous.
pub fn annot_val_to_slot(value: &[u8]) -> B256 {
    debug_assert!(
        value.len() <= EVM_WORD_LENGTH,
        "annot_val_to_slot: value too long ({} bytes)",
        value.len()
    );
    let mut buf = [0u8; EVM_WORD_LENGTH];
    buf[..value.len()].copy_from_slice(value);
    B256::from(buf)
}

/// Encode "a value of length `len` is present" as a B+ tree **value** word — the
/// length `+ 1` as a **right**-aligned u32 (its low 4 bytes, for a 32-byte word):
///
/// ```text
///   byte:  0 .............. 28 .............. 32
///          | 0 (padding)      | len + 1 (u32 be) |
/// ```
///
/// The `+ 1` is what lets `0` mean "absent" (a never-written or lazy-deleted slot)
/// while still recording a genuinely empty value: `slot_presence(0)` = `1`.
pub fn slot_presence(len: usize) -> B256 {
    let mut buf = [0u8; EVM_WORD_LENGTH];
    buf[EVM_WORD_LENGTH - size_of::<u32>()..].copy_from_slice(&(len as u32 + 1).to_be_bytes());
    B256::from(buf)
}

/// Decode a value's original length from a presence word, or `None` if absent —
/// the exact inverse of [`slot_presence`].
pub fn slot_to_val_len(word: B256) -> Option<usize> {
    let n = u32::from_be_bytes(
        word.0[EVM_WORD_LENGTH - size_of::<u32>()..]
            .try_into()
            .unwrap(),
    );
    if n == 0 { None } else { Some((n - 1) as usize) }
}

/// Which side of `bound` a range predicate keeps, and whether `bound` itself is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Strictly greater than the bound.
    Gt,
    /// Greater than or equal to the bound.
    Gte,
    /// Strictly less than the bound.
    Lt,
    /// Less than or equal to the bound.
    Lte,
}

/// Record that `attr` currently has `value` (≤ 32 bytes) on some entity.
///
/// Idempotent: the value is a distinct key in the tree, so re-recording it just
/// rewrites the same presence word. Mirrors v1's `tier2_insert` (int mode).
pub fn insert<S: IndexStorage>(
    storage: &mut S,
    attr: &[u8],
    ty: AttributeType,
    value: &[u8],
) -> Result<(), S::Error> {
    btree::insert(
        storage,
        btree_header_address(attr, ty),
        annot_val_to_slot(value),
        slot_presence(value.len()),
    )
}

/// Record that `attr` no longer has `value` (lazy — the key stays, marked absent).
/// Mirrors v1's `tier2_remove` (int mode).
pub fn remove<S: IndexStorage>(
    storage: &mut S,
    attr: &[u8],
    ty: AttributeType,
    value: &[u8],
) -> Result<(), S::Error> {
    btree::lazy_delete(
        storage,
        btree_header_address(attr, ty),
        annot_val_to_slot(value),
    )
}

/// The values of `attr` on the `kind` side of `bound`, in ascending order.
///
/// Ported from v1's `int_range_values`. For `Gt`/`Gte` it descends to `bound` and
/// walks forward; for `Lt`/`Lte` it walks from the start and stops at `bound`.
pub fn scan<S: IndexStorage>(
    storage: &mut S,
    attr: &[u8],
    ty: AttributeType,
    bound: &[u8],
    kind: Bound,
) -> Result<Vec<Vec<u8>>, S::Error> {
    let header = btree_header_address(attr, ty);
    let bound_slot = annot_val_to_slot(bound);
    let mut result = Vec::new();

    match kind {
        Bound::Gt | Bound::Gte => {
            let inclusive = matches!(kind, Bound::Gte);
            for (slot_key, slot_val) in btree::iter_from(storage, header, bound_slot)? {
                let Some(len) = slot_to_val_len(slot_val) else {
                    continue;
                };
                if !inclusive && slot_key == bound_slot {
                    continue;
                }
                result.push(slot_key.0[..len].to_vec());
            }
        }
        Bound::Lt | Bound::Lte => {
            let inclusive = matches!(kind, Bound::Lte);
            for (slot_key, slot_val) in btree::iter_from(storage, header, B256::ZERO)? {
                if slot_key > bound_slot {
                    break;
                }
                if !inclusive && slot_key == bound_slot {
                    break;
                }
                let Some(len) = slot_to_val_len(slot_val) else {
                    continue;
                };
                result.push(slot_key.0[..len].to_vec());
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indices::storage::MemStorage;

    const TY: AttributeType = AttributeType::U256;

    #[test]
    fn presence_encoding_roundtrips_length() {
        assert_eq!(slot_to_val_len(slot_presence(0)), Some(0));
        assert_eq!(slot_to_val_len(slot_presence(11)), Some(11));
        assert_eq!(slot_to_val_len(B256::ZERO), None);
    }

    #[test]
    fn slot_key_is_left_aligned_and_zero_padded() {
        let k = annot_val_to_slot(b"hi");
        assert_eq!(&k.0[..2], b"hi");
        assert_eq!(&k.0[2..], &[0u8; EVM_WORD_LENGTH - 2]);
    }

    #[test]
    fn slot_key_preserves_numeric_order_for_fixed_width_values() {
        // Fixed-width big-endian keys sort the same as the numbers they encode.
        assert!(annot_val_to_slot(&10u64.to_be_bytes()) < annot_val_to_slot(&20u64.to_be_bytes()));
    }

    /// Insert a set of block numbers, then range-scan each `Bound`.
    #[test]
    fn scans_each_bound() {
        let mut s = MemStorage::default();
        let attr = b"$expiration";
        let vals = [10u64, 20, 30, 40, 50];
        for v in vals {
            insert(&mut s, attr, TY, &v.to_be_bytes()).unwrap();
        }
        let as_u64 = |rows: Vec<Vec<u8>>| {
            rows.into_iter()
                .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
                .collect::<Vec<_>>()
        };
        let b30 = 30u64.to_be_bytes();
        assert_eq!(
            as_u64(scan(&mut s, attr, TY, &b30, Bound::Gt).unwrap()),
            vec![40, 50]
        );
        assert_eq!(
            as_u64(scan(&mut s, attr, TY, &b30, Bound::Gte).unwrap()),
            vec![30, 40, 50]
        );
        assert_eq!(
            as_u64(scan(&mut s, attr, TY, &b30, Bound::Lt).unwrap()),
            vec![10, 20]
        );
        assert_eq!(
            as_u64(scan(&mut s, attr, TY, &b30, Bound::Lte).unwrap()),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn removed_values_drop_out_of_scans() {
        let mut s = MemStorage::default();
        let attr = b"$expiration";
        for v in [10u64, 20, 30] {
            insert(&mut s, attr, TY, &v.to_be_bytes()).unwrap();
        }
        remove(&mut s, attr, TY, &20u64.to_be_bytes()).unwrap();
        let rows = scan(&mut s, attr, TY, &0u64.to_be_bytes(), Bound::Gte).unwrap();
        let got: Vec<u64> = rows
            .into_iter()
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(got, vec![10, 30]);
    }

    #[test]
    fn reconstructs_variable_length_string_values() {
        // Even though this index is "int-mode", the key is just right-padded bytes;
        // the presence word carries the true length so short values round-trip.
        let mut s = MemStorage::default();
        let attr = b"tag";
        insert(&mut s, attr, TY, b"ab").unwrap();
        insert(&mut s, attr, TY, b"abc").unwrap();
        let rows = scan(&mut s, attr, TY, b"", Bound::Gte).unwrap();
        assert_eq!(rows, vec![b"ab".to_vec(), b"abc".to_vec()]);
    }
}
