//! The tier-2 **str-mode** index: a cascade for string values up to 128 bytes.
//!
//! The [`btree`](crate::btree) range index keys on a single 32-byte word, so it
//! can't order values longer than that. The cascade handles the rest by indexing a
//! value **chunk by chunk**: level 0 stores its first 32-byte chunk, level 1 stores
//! the second chunk under the account named by the first, and so on
//! ([`str_level_address`](crate::address::str_level_address)). A range or glob scan
//! walks the levels depth-first, reconstructing each stored value.
//!
//! Each level account is an unordered EVM-storage map, so — as elsewhere in the
//! index — it carries a companion **enumeration list**
//! ([`list_address_for`](crate::address::list_address_for)) recording every distinct
//! chunk written to it, which is how the reader knows what's there.
//!
//! Encoding is **consensus-critical** and ported verbatim from `arkiv-db-engine`.
//! Deletion is *lazy*: [`remove`] zeroes a value's presence slots but never prunes
//! list entries; the reader skips zeroed slots.

use alloy_primitives::{Address, B256};

use arkiv_constants::WORD_LEN;

use crate::address::{list_address_for, str_level_address};
use crate::range::{Bound, slot_presence, slot_to_val_len};
use crate::slot::{storage_to_u64, u64_to_storage};
use crate::storage::IndexStorage;

/// The longest string value the cascade indexes: four 32-byte chunks.
pub const MAX_STR_LEN: usize = 4 * WORD_LEN;

/// Split a value (≤ [`MAX_STR_LEN`] bytes) into 32-byte right-padded chunks. An
/// empty value produces a single zero chunk (so it still occupies a level-0 slot).
pub fn value_chunks(value: &[u8]) -> Vec<B256> {
    debug_assert!(
        value.len() <= MAX_STR_LEN,
        "value_chunks: value too long ({} bytes)",
        value.len()
    );
    if value.is_empty() {
        return vec![B256::ZERO];
    }
    let chunk_count = value.len().div_ceil(WORD_LEN);
    let mut chunks = Vec::with_capacity(chunk_count);
    for chunk_index in 0..chunk_count {
        let start = chunk_index * WORD_LEN;
        let end = (start + WORD_LEN).min(value.len());
        let mut chunk = [0u8; WORD_LEN];
        chunk[..end - start].copy_from_slice(&value[start..end]);
        chunks.push(B256::from(chunk));
    }
    chunks
}

/// Record that `attr` currently has string `value` on some entity.
///
/// Walks the value's chunks, writing each one's presence at its level and appending
/// it to that level's list the first time it appears there (many values can share an
/// intermediate-level chunk). Mirrors v1's `tier2_insert` (str mode).
pub fn insert<Storage: IndexStorage>(
    storage: &mut Storage,
    attr: &[u8],
    value: &[u8],
) -> Result<(), Storage::Error> {
    let presence = slot_presence(value.len());
    let mut prefix: Vec<u8> = Vec::new();
    for chunk in value_chunks(value) {
        let level_address = str_level_address(attr, &prefix);
        storage.ensure_account_persists(level_address)?;
        if storage.storage(level_address, chunk)? == B256::ZERO {
            list_append(storage, level_address, chunk)?;
        }
        storage.set_storage(level_address, chunk, presence)?;
        prefix.extend_from_slice(chunk.as_slice());
    }
    Ok(())
}

/// Record that `attr` no longer has string `value` (lazy — presence slots are zeroed,
/// list entries stay). Mirrors v1's `tier2_remove` (str mode).
pub fn remove<Storage: IndexStorage>(
    storage: &mut Storage,
    attr: &[u8],
    value: &[u8],
) -> Result<(), Storage::Error> {
    let mut prefix: Vec<u8> = Vec::new();
    for chunk in value_chunks(value) {
        let level_address = str_level_address(attr, &prefix);
        storage.set_storage(level_address, chunk, B256::ZERO)?;
        prefix.extend_from_slice(chunk.as_slice());
    }
    Ok(())
}

/// The string values of `attr` on the `kind` side of `bound`.
pub fn scan<Storage: IndexStorage>(
    storage: &mut Storage,
    attr: &[u8],
    bound: &[u8],
    kind: Bound,
) -> Result<Vec<Vec<u8>>, Storage::Error> {
    let mut values = Vec::new();
    collect_all(storage, attr, &[], &mut values)?;
    values.retain(|value| matches_bound(kind, value, bound));
    Ok(values)
}

/// The string values of `attr` that start with `prefix`.
pub fn glob<Storage: IndexStorage>(
    storage: &mut Storage,
    attr: &[u8],
    prefix: &[u8],
) -> Result<Vec<Vec<u8>>, Storage::Error> {
    let mut values = Vec::new();
    collect_all(storage, attr, &[], &mut values)?;
    values.retain(|value| value.starts_with(prefix));
    Ok(values)
}

fn matches_bound(kind: Bound, value: &[u8], bound: &[u8]) -> bool {
    match kind {
        Bound::Gt => value > bound,
        Bound::Gte => value >= bound,
        Bound::Lt => value < bound,
        Bound::Lte => value <= bound,
    }
}

/// Depth-first walk of the cascade under `prefix`, reconstructing each full value.
///
/// At each level a slot is either a **leaf** (its recorded length ends within this
/// chunk — reconstruct `prefix + chunk[..tail]`) or an **internal** node (a longer
/// value continues — recurse with `prefix + chunk`).
fn collect_all<Storage: IndexStorage>(
    storage: &mut Storage,
    attr: &[u8],
    prefix: &[u8],
    values: &mut Vec<Vec<u8>>,
) -> Result<(), Storage::Error> {
    let level = prefix.len() / WORD_LEN;
    let level_address = str_level_address(attr, prefix);
    for (chunk, presence) in list_entries(storage, level_address)? {
        let Some(total_len) = slot_to_val_len(presence) else {
            continue; // zeroed (removed) slot
        };
        if total_len <= (level + 1) * WORD_LEN {
            let tail_len = total_len - level * WORD_LEN;
            let mut value = prefix.to_vec();
            value.extend_from_slice(&chunk.0[..tail_len]);
            values.push(value);
        } else {
            let mut next_prefix = prefix.to_vec();
            next_prefix.extend_from_slice(chunk.as_slice());
            collect_all(storage, attr, &next_prefix, values)?;
        }
    }
    Ok(())
}

// ── Enumeration list (a level account's distinct chunks) ───────────────

/// Slot in a list account holding the `index`-th entry (1-based); slot 0 is the count.
fn list_entry_slot(index: u64) -> B256 {
    u64_to_storage(index)
}

/// Append `entry` to `index_address`'s enumeration list.
fn list_append<Storage: IndexStorage>(
    storage: &mut Storage,
    index_address: Address,
    entry: B256,
) -> Result<(), Storage::Error> {
    let list_address = list_address_for(index_address);
    storage.ensure_account_persists(list_address)?;
    let count = storage_to_u64(storage.storage(list_address, B256::ZERO)?);
    storage.set_storage(list_address, list_entry_slot(count + 1), entry)?;
    storage.set_storage(list_address, B256::ZERO, u64_to_storage(count + 1))?;
    Ok(())
}

/// Every live `(chunk, presence)` entry of a level account, ascending by chunk. A
/// zeroed presence (a removed value) is skipped.
fn list_entries<Storage: IndexStorage>(
    storage: &mut Storage,
    index_address: Address,
) -> Result<Vec<(B256, B256)>, Storage::Error> {
    let list_address = list_address_for(index_address);
    let count = storage_to_u64(storage.storage(list_address, B256::ZERO)?);
    let mut entries = Vec::with_capacity(count as usize);
    for entry_index in 1..=count {
        let chunk = storage.storage(list_address, list_entry_slot(entry_index))?;
        let presence = storage.storage(index_address, chunk)?;
        if presence != B256::ZERO {
            entries.push((chunk, presence));
        }
    }
    entries.sort_unstable_by_key(|(chunk, _)| *chunk);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemStorage;

    fn sorted(mut values: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        values.sort();
        values
    }

    #[test]
    fn value_chunks_pads_and_splits() {
        assert_eq!(value_chunks(b""), vec![B256::ZERO]);
        assert_eq!(value_chunks(b"hi").len(), 1);
        assert_eq!(value_chunks(b"hi")[0].0[..2], *b"hi");
        // 40 bytes spans two chunks.
        assert_eq!(value_chunks(&[7u8; 40]).len(), 2);
    }

    #[test]
    fn short_values_scan_and_glob() {
        let mut storage = MemStorage::default();
        let attr = b"fruit";
        for value in [b"apple".as_slice(), b"banana", b"cherry"] {
            insert(&mut storage, attr, value).unwrap();
        }
        assert_eq!(
            sorted(scan(&mut storage, attr, b"banana", Bound::Gt).unwrap()),
            vec![b"cherry".to_vec()],
        );
        assert_eq!(
            sorted(scan(&mut storage, attr, b"banana", Bound::Gte).unwrap()),
            vec![b"banana".to_vec(), b"cherry".to_vec()],
        );
        assert_eq!(
            sorted(scan(&mut storage, attr, b"banana", Bound::Lt).unwrap()),
            vec![b"apple".to_vec()],
        );
        assert_eq!(
            glob(&mut storage, attr, b"ba").unwrap(),
            vec![b"banana".to_vec()],
        );
    }

    #[test]
    fn long_multi_level_values_round_trip() {
        let mut storage = MemStorage::default();
        let attr = b"name";
        // Two values sharing a 40-byte prefix, so they diverge only at level 1.
        let mut a = vec![b'x'; 40];
        a.extend_from_slice(b"-alpha");
        let mut b = vec![b'x'; 40];
        b.extend_from_slice(b"-beta");
        insert(&mut storage, attr, &a).unwrap();
        insert(&mut storage, attr, &b).unwrap();

        // Both come back via a prefix glob that only matches at a deeper level.
        assert_eq!(
            sorted(glob(&mut storage, attr, &[b'x'; 40]).unwrap()),
            sorted(vec![a.clone(), b.clone()]),
        );
        assert_eq!(glob(&mut storage, attr, &a).unwrap(), vec![a.clone()]);
    }

    #[test]
    fn removed_values_drop_out() {
        let mut storage = MemStorage::default();
        let attr = b"fruit";
        insert(&mut storage, attr, b"apple").unwrap();
        insert(&mut storage, attr, b"banana").unwrap();
        remove(&mut storage, attr, b"apple").unwrap();
        assert_eq!(
            scan(&mut storage, attr, b"", Bound::Gte).unwrap(),
            vec![b"banana".to_vec()],
        );
    }
}
