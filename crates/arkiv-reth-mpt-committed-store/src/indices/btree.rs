//! The tier-2 int-mode index: a B+ tree laid out in account storage slots.
//!
//! reth's storage is an unordered slot map, but range queries (`$expiration >
//! N`, …) need the *values* of an attribute in sorted order. This is that ordered
//! structure, built by hand inside the slots: a B+ tree whose leaves are chained
//! left-to-right so a range scan is a descend-then-walk. It backs attributes whose
//! values fit in 32 bytes (block numbers, uint attributes); longer string values
//! use the cascade (a later module) instead.
//!
//! Layout, per [`btree_header_address`](crate::indices::address::btree_header_address) /
//! [`btree_node_address`](crate::indices::address::btree_node_address):
//! - **Header** (slot 0 of the header account): `[0..8]` root node id, `[8..16]`
//!   next id to allocate, `[16]` = [`BTREE_MAGIC`] — all big-endian.
//! - **Node** (in its own account): slot 0 meta = `[0..8]` right-sibling id,
//!   `[8..10]` key count (u16), `[10]` leaf bit; keys at
//!   [`key_slot`]; values at [`value_slot`]. A leaf holds `key_count` values
//!   (presence markers); an internal node holds `key_count + 1` child ids.
//!
//! **Consensus-critical.** Every byte here — node fan-out, split point, slot
//! numbering, meta packing — feeds the state root. It is ported verbatim from
//! `arkiv-db-engine`; do not "tidy" the encoding.
//!
//! Deletion is *lazy*: [`lazy_delete`] zeroes a leaf value slot but never removes
//! keys or merges nodes, so the tree only grows. [`iter_from`] and re-insert both
//! treat a zero value slot as absent.

use alloy_primitives::{Address, B256};

use arkiv_interfaces::constants::WORD_LEN;

use crate::indices::address::{BTREE_MAGIC, BTREE_ORDER, btree_node_address};
use crate::indices::slot::{storage_to_u64, u64_to_storage};
use crate::indices::storage::IndexStorage;

// ── Slot numbering within a node account ──────────────────────────────

/// Slot 0 of a node holds its meta word; the `i`-th key lives at
/// `KEYS_BASE_SLOT + i`, and the `i`-th value/child at `VALUES_BASE_SLOT + i`
/// (values follow all `BTREE_ORDER` key slots).
const KEYS_BASE_SLOT: u64 = 1;
const VALUES_BASE_SLOT: u64 = KEYS_BASE_SLOT + BTREE_ORDER as u64;

// Layout invariants, checked at compile time so a stray edit to the node order or a
// field type can't silently corrupt the on-chain encoding:
const _: () = assert!(
    size_of::<u64>() + size_of::<u16>() + size_of::<u8>() <= WORD_LEN,
    "the node meta word (u64 sibling id + u16 key count + u8 leaf flag) must fit one word",
);
const _: () = assert!(
    2 * size_of::<u64>() < WORD_LEN,
    "the header word (root id + next id + a magic byte) must fit one word",
);
const _: () = assert!(
    BTREE_ORDER < u16::MAX as usize,
    "a splitting node holds up to BTREE_ORDER + 1 keys, and the count is a u16",
);

/// Slot holding the `i`-th key.
#[inline]
fn key_slot(i: usize) -> B256 {
    u64_to_storage(KEYS_BASE_SLOT + i as u64)
}

/// Slot holding the `i`-th value/child.
#[inline]
fn value_slot(i: usize) -> B256 {
    u64_to_storage(VALUES_BASE_SLOT + i as u64)
}

/// A decoded B+ tree node. Leaves store presence markers in `values`; internal
/// nodes store child node ids (big-endian in the low 8 bytes of each `B256`).
struct Node {
    is_leaf: bool,
    right_sibling: u64,
    keys: Vec<B256>,
    values: Vec<B256>,
}

// ── Header ────────────────────────────────────────────────────────────
//
// Header word — slot 0 of the header account. `M` = BTREE_MAGIC:
//
//   byte:  0 ............. 8 ............. 16   17 ......... 32
//          | root_id (u64) | next_id (u64) | M | 0 (unused)   |
//
// `root_id` is the current root node id (0 ⇒ empty tree); `next_id` is the next
// node id to hand out; `M` tags this account as a B+ tree header (a string-index
// list account instead holds a plain u64 count here, so its byte 16 is 0).

/// Read `(root_id, next_id)` from the header word (see the section comment). Both
/// are 0 on a fresh tree.
fn read_header<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
) -> Result<(u64, u64), S::Error> {
    let raw = storage.storage(header_addr, B256::ZERO)?;
    let root_id = u64::from_be_bytes(raw.0[0..8].try_into().unwrap());
    let next_id = u64::from_be_bytes(raw.0[8..16].try_into().unwrap());
    Ok((root_id, next_id))
}

/// Write the header word (see the section comment), stamping [`BTREE_MAGIC`].
fn write_header<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    root_id: u64,
    next_id: u64,
) -> Result<(), S::Error> {
    let mut buf = [0u8; WORD_LEN];
    buf[0..8].copy_from_slice(&root_id.to_be_bytes());
    buf[8..16].copy_from_slice(&next_id.to_be_bytes());
    buf[16] = BTREE_MAGIC;
    storage.ensure_account_persists(header_addr)?;
    storage.set_storage(header_addr, B256::ZERO, B256::from(buf))
}

/// Allocate a fresh node id: read `next_id`, bump it (preserving `root_id`), return
/// the allocated id. The first id handed out is 1 (0 means "no node").
fn alloc_node<S: IndexStorage>(storage: &mut S, header_addr: Address) -> Result<u64, S::Error> {
    let (root_id, next_id) = read_header(storage, header_addr)?;
    let node_id = if next_id == 0 { 1 } else { next_id };
    write_header(storage, header_addr, root_id, node_id + 1)?;
    Ok(node_id)
}

// ── Node read / write ─────────────────────────────────────────────────
//
// Each node lives in its own account (at `btree_node_address`). Its slot 0 is a
// meta word; keys and values follow in later slots:
//
//   meta word:  0 ................ 8 .......... 10   11 ....... 32
//               | right_sibling id | key_count | leaf | 0 ...    |
//               |      (u64)       |   (u16)   | (u8) |          |
//
//   slot KEYS_BASE_SLOT + i     → the i-th key
//   slot VALUES_BASE_SLOT + i   → the i-th value (leaf: presence marker;
//                                 internal: child node id, u64-encoded)
//
// `right_sibling` chains leaves left-to-right for range scans (0 = last leaf).
// A leaf stores `key_count` values; an internal node stores `key_count + 1`.

/// Read the node with id `node_id`: decode its meta word, then its keys and values
/// from the following slots (see the section comment for the layout).
fn read_node<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    node_id: u64,
) -> Result<Node, S::Error> {
    let node_addr = btree_node_address(header_addr, node_id);
    let meta = storage.storage(node_addr, B256::ZERO)?;
    let right_sibling = u64::from_be_bytes(meta.0[0..8].try_into().unwrap());
    let key_count = u16::from_be_bytes(meta.0[8..10].try_into().unwrap()) as usize;
    let is_leaf = meta.0[10] & 1 != 0;

    let mut keys = Vec::with_capacity(key_count);
    for i in 0..key_count {
        keys.push(storage.storage(node_addr, key_slot(i))?);
    }
    // Leaves: key_count values; internals: key_count + 1 children.
    let val_count = if is_leaf { key_count } else { key_count + 1 };
    let mut values = Vec::with_capacity(val_count);
    for i in 0..val_count {
        values.push(storage.storage(node_addr, value_slot(i))?);
    }
    Ok(Node {
        is_leaf,
        right_sibling,
        keys,
        values,
    })
}

/// Write `node` to id `node_id`: encode the meta word, then the keys and values
/// into the following slots (see the section comment for the layout).
fn write_node<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    node_id: u64,
    node: &Node,
) -> Result<(), S::Error> {
    let node_addr = btree_node_address(header_addr, node_id);
    storage.ensure_account_persists(node_addr)?;
    let mut meta = [0u8; WORD_LEN];
    meta[0..8].copy_from_slice(&node.right_sibling.to_be_bytes());
    meta[8..10].copy_from_slice(&(node.keys.len() as u16).to_be_bytes());
    meta[10] = if node.is_leaf { 1 } else { 0 };
    storage.set_storage(node_addr, B256::ZERO, B256::from(meta))?;
    for (i, k) in node.keys.iter().enumerate() {
        storage.set_storage(node_addr, key_slot(i), *k)?;
    }
    for (i, v) in node.values.iter().enumerate() {
        storage.set_storage(node_addr, value_slot(i), *v)?;
    }
    Ok(())
}

fn descend_to_leaf<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    mut node_id: u64,
    key: B256,
) -> Result<u64, S::Error> {
    loop {
        let node = read_node(storage, header_addr, node_id)?;
        if node.is_leaf {
            return Ok(node_id);
        }
        // Among children, `k <= key` counts how many keys precede the target
        // child; that count is the correct child index.
        let pos = node.keys.partition_point(|k| *k <= key);
        node_id = storage_to_u64(node.values[pos]);
    }
}

// ── Insert ────────────────────────────────────────────────────────────

enum InsertResult {
    Done,
    Split { push_up: B256, new_sibling_id: u64 },
}

fn insert_recursive<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    node_id: u64,
    key: B256,
    value: B256,
) -> Result<InsertResult, S::Error> {
    let mut node = read_node(storage, header_addr, node_id)?;

    if node.is_leaf {
        let pos = node.keys.partition_point(|k| *k < key);
        if pos < node.keys.len() && node.keys[pos] == key {
            // Update in place (handles lazy-delete re-insert and the B256::ZERO key).
            node.values[pos] = value;
            write_node(storage, header_addr, node_id, &node)?;
            return Ok(InsertResult::Done);
        }
        node.keys.insert(pos, key);
        node.values.insert(pos, value);
        if node.keys.len() <= BTREE_ORDER {
            write_node(storage, header_addr, node_id, &node)?;
            return Ok(InsertResult::Done);
        }
        let mid = node.keys.len() / 2;
        let new_id = alloc_node(storage, header_addr)?;
        let new_node = Node {
            is_leaf: true,
            right_sibling: node.right_sibling,
            keys: node.keys.split_off(mid),
            values: node.values.split_off(mid),
        };
        let push_up = new_node.keys[0];
        node.right_sibling = new_id;
        write_node(storage, header_addr, node_id, &node)?;
        write_node(storage, header_addr, new_id, &new_node)?;
        return Ok(InsertResult::Split {
            push_up,
            new_sibling_id: new_id,
        });
    }

    // Internal node.
    let pos = node.keys.partition_point(|k| *k <= key);
    let child_id = storage_to_u64(node.values[pos]);
    match insert_recursive(storage, header_addr, child_id, key, value)? {
        InsertResult::Done => Ok(InsertResult::Done),
        InsertResult::Split {
            push_up,
            new_sibling_id,
        } => {
            node.keys.insert(pos, push_up);
            // A child pointer is the child's node id, encoded like any u64 value.
            node.values.insert(pos + 1, u64_to_storage(new_sibling_id));
            if node.keys.len() <= BTREE_ORDER {
                write_node(storage, header_addr, node_id, &node)?;
                return Ok(InsertResult::Done);
            }
            let mid = node.keys.len() / 2;
            let push_up_key = node.keys[mid];
            let new_id = alloc_node(storage, header_addr)?;
            let new_node = Node {
                is_leaf: false,
                right_sibling: node.right_sibling,
                keys: node.keys.split_off(mid + 1),
                values: node.values.split_off(mid + 1),
            };
            node.keys.truncate(mid);
            node.right_sibling = new_id;
            write_node(storage, header_addr, node_id, &node)?;
            write_node(storage, header_addr, new_id, &new_node)?;
            Ok(InsertResult::Split {
                push_up: push_up_key,
                new_sibling_id: new_id,
            })
        }
    }
}

/// Insert `(key, value)`, or update the value in place if `key` is already present.
/// Grows the tree by one level when the root splits.
pub fn insert<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    key: B256,
    value: B256,
) -> Result<(), S::Error> {
    let (root_id, _) = read_header(storage, header_addr)?;
    if root_id == 0 {
        let leaf_id = alloc_node(storage, header_addr)?;
        let leaf = Node {
            is_leaf: true,
            right_sibling: 0,
            keys: vec![key],
            values: vec![value],
        };
        write_node(storage, header_addr, leaf_id, &leaf)?;
        let (_, next) = read_header(storage, header_addr)?;
        return write_header(storage, header_addr, leaf_id, next);
    }
    match insert_recursive(storage, header_addr, root_id, key, value)? {
        InsertResult::Done => {}
        InsertResult::Split {
            push_up,
            new_sibling_id,
        } => {
            let new_root_id = alloc_node(storage, header_addr)?;
            // Two child pointers: the old root (now the left child) and its new sibling.
            let new_root = Node {
                is_leaf: false,
                right_sibling: 0,
                keys: vec![push_up],
                values: vec![u64_to_storage(root_id), u64_to_storage(new_sibling_id)],
            };
            write_node(storage, header_addr, new_root_id, &new_root)?;
            let (_, next) = read_header(storage, header_addr)?;
            write_header(storage, header_addr, new_root_id, next)?;
        }
    }
    Ok(())
}

/// Mark `key` absent by zeroing its leaf value slot. Keys are never physically
/// removed and nodes never merge — a later insert of the same key overwrites the
/// zero in place.
pub fn lazy_delete<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    key: B256,
) -> Result<(), S::Error> {
    let (root_id, _) = read_header(storage, header_addr)?;
    if root_id == 0 {
        return Ok(());
    }
    let leaf_id = descend_to_leaf(storage, header_addr, root_id, key)?;
    let node = read_node(storage, header_addr, leaf_id)?;
    let pos = node.keys.partition_point(|k| *k < key);
    if pos < node.keys.len() && node.keys[pos] == key {
        let node_addr = btree_node_address(header_addr, leaf_id);
        storage.set_storage(node_addr, value_slot(pos), B256::ZERO)?;
    }
    Ok(())
}

/// Every live `(key, value)` with `key >= from`, in ascending key order.
/// Lazy-deleted entries (zero value slot) are skipped.
pub fn iter_from<S: IndexStorage>(
    storage: &mut S,
    header_addr: Address,
    from: B256,
) -> Result<Vec<(B256, B256)>, S::Error> {
    let (root_id, _) = read_header(storage, header_addr)?;
    if root_id == 0 {
        return Ok(vec![]);
    }
    let mut leaf_id = descend_to_leaf(storage, header_addr, root_id, from)?;
    let mut result = Vec::new();
    let mut first = true;
    loop {
        let leaf = read_node(storage, header_addr, leaf_id)?;
        let start = if first {
            first = false;
            leaf.keys.partition_point(|k| *k < from)
        } else {
            0
        };
        for i in start..leaf.keys.len() {
            if leaf.values[i] != B256::ZERO {
                result.push((leaf.keys[i], leaf.values[i]));
            }
        }
        if leaf.right_sibling == 0 {
            break;
        }
        leaf_id = leaf.right_sibling;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indices::address::btree_header_address;
    use crate::indices::storage::MemStorage;
    use arkiv_interfaces::entity::AttributeType;

    fn slot(n: u64) -> B256 {
        // A distinct, order-preserving key for id `n` (big-endian in the low bytes).
        u64_to_storage(n)
    }

    /// Non-zero presence marker (a real caller stores `len + 1`; any non-zero works
    /// for the structural tests here).
    fn present() -> B256 {
        u64_to_storage(1)
    }

    fn keys_in_order<S: IndexStorage>(storage: &mut S, hdr: Address) -> Vec<u64> {
        iter_from(storage, hdr, B256::ZERO)
            .unwrap()
            .into_iter()
            .map(|(k, _)| storage_to_u64(k))
            .collect()
    }

    #[test]
    fn empty_tree_iterates_empty() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        assert!(iter_from(&mut s, hdr, B256::ZERO).unwrap().is_empty());
    }

    #[test]
    fn single_insert_roundtrips() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        insert(&mut s, hdr, slot(42), present()).unwrap();
        assert_eq!(keys_in_order(&mut s, hdr), vec![42]);
    }

    #[test]
    fn stays_sorted_regardless_of_insert_order() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        for n in [5u64, 1, 9, 3, 7, 2, 8, 4, 6] {
            insert(&mut s, hdr, slot(n), present()).unwrap();
        }
        assert_eq!(keys_in_order(&mut s, hdr), vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    /// Past `BTREE_ORDER` the root leaf must split and the tree grows a level; the
    /// leaf chain still yields every key in order. This is the core invariant.
    #[test]
    fn splits_past_order_and_keeps_all_keys_ordered() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        let n = (BTREE_ORDER as u64) * 4 + 1; // force several splits / a taller tree
        // Insert descending so splits happen on the busy end.
        for k in (1..=n).rev() {
            insert(&mut s, hdr, slot(k), present()).unwrap();
        }
        // Root is no longer a leaf: header's root id points past the first node.
        let (root_id, _) = read_header(&mut s, hdr).unwrap();
        assert!(!read_node(&mut s, hdr, root_id).unwrap().is_leaf);
        assert_eq!(keys_in_order(&mut s, hdr), (1..=n).collect::<Vec<_>>());
    }

    #[test]
    fn iter_from_respects_lower_bound() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        for n in 1..=50u64 {
            insert(&mut s, hdr, slot(n), present()).unwrap();
        }
        let got: Vec<u64> = iter_from(&mut s, hdr, slot(40))
            .unwrap()
            .into_iter()
            .map(|(k, _)| storage_to_u64(k))
            .collect();
        assert_eq!(got, (40..=50).collect::<Vec<_>>());
    }

    #[test]
    fn lazy_delete_hides_key() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        for n in [1u64, 2, 3] {
            insert(&mut s, hdr, slot(n), present()).unwrap();
        }
        lazy_delete(&mut s, hdr, slot(2)).unwrap();
        assert_eq!(keys_in_order(&mut s, hdr), vec![1, 3]);
    }

    #[test]
    fn reinsert_after_delete_restores_key() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        insert(&mut s, hdr, slot(2), present()).unwrap();
        lazy_delete(&mut s, hdr, slot(2)).unwrap();
        assert!(keys_in_order(&mut s, hdr).is_empty());
        insert(&mut s, hdr, slot(2), present()).unwrap();
        assert_eq!(keys_in_order(&mut s, hdr), vec![2]);
    }

    #[test]
    fn duplicate_insert_updates_in_place() {
        let mut s = MemStorage::default();
        let hdr = btree_header_address(b"k", AttributeType::U256);
        insert(&mut s, hdr, slot(7), u64_to_storage(1)).unwrap();
        insert(&mut s, hdr, slot(7), u64_to_storage(2)).unwrap();
        let entries = iter_from(&mut s, hdr, B256::ZERO).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, u64_to_storage(2));
    }

    /// Two attributes' trees live at different header addresses and don't interfere.
    #[test]
    fn distinct_trees_are_independent() {
        let mut s = MemStorage::default();
        let a = btree_header_address(b"attrA", AttributeType::U256);
        let b = btree_header_address(b"attrB", AttributeType::U256);
        insert(&mut s, a, slot(1), present()).unwrap();
        insert(&mut s, b, slot(2), present()).unwrap();
        assert_eq!(keys_in_order(&mut s, a), vec![1]);
        assert_eq!(keys_in_order(&mut s, b), vec![2]);
    }
}
