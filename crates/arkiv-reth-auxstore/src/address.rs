//! The keccak-derived account addresses that hold the index.
//!
//! Each index bucket lives at its own account address, derived by hashing a
//! domain-tagged, separated encoding of the thing it indexes. The domain tag
//! (`b"arkiv.pair"`, …) keeps the different bucket kinds in disjoint regions of
//! the address space, and the `0x00` separators keep `(key, value)` pairs from
//! colliding across a shared prefix boundary.
//!
//! **These derivations are consensus-critical.** The address is where a bucket's
//! contents are committed in the trie, so a change to any tag, separator, or byte
//! order changes the state root and forks the chain. They are ported verbatim from
//! `arkiv-db-engine` and locked with golden vectors in the tests below — never
//! adjust them to "clean up" the encoding.
//!
//! Every derivation keeps 20 of a 32-byte keccak. Not because these are Ethereum
//! addresses — no bucket has a key or a signer — but because reth keys accounts by
//! 20 bytes. That costs 96 bits, and undoing it needs a wider account key, which
//! the golden vectors below correctly treat as a fork.

use alloy_primitives::{Address, keccak256};
use arkiv_interfaces::constants::ETH_ADDRESS_LEN;
use arkiv_interfaces::entity::AttributeType;

/// Every domain tag is this many bytes (`b"arkiv.pair"`, `b"arkiv.ibth"`, …), so
/// the fixed-size node buffer below can be sized from it.
const DOMAIN_TAG_LEN: usize = 10;

/// Domain tag for a tier-1 equality (pair) bucket.
const PAIR_DOMAIN: &[u8] = b"arkiv.pair";

/// Domain tag for a tier-2 int-mode B+ tree header account.
const BTREE_HEADER_DOMAIN: &[u8] = b"arkiv.ibth";

/// Domain tag for a tier-2 int-mode B+ tree node account.
const BTREE_NODE_DOMAIN: &[u8] = b"arkiv.ibtn";

/// Domain tag for a tier-2 str-mode cascade level account.
const STR_LEVEL_DOMAIN: &[u8] = b"arkiv.sidx";

/// Domain tag for an enumeration-list account (a cascade level's live keys).
const LIST_DOMAIN: &[u8] = b"arkiv.list";

const _: () = assert!(
    PAIR_DOMAIN.len() == DOMAIN_TAG_LEN
        && BTREE_HEADER_DOMAIN.len() == DOMAIN_TAG_LEN
        && BTREE_NODE_DOMAIN.len() == DOMAIN_TAG_LEN
        && STR_LEVEL_DOMAIN.len() == DOMAIN_TAG_LEN
        && LIST_DOMAIN.len() == DOMAIN_TAG_LEN,
    "every domain tag must be DOMAIN_TAG_LEN bytes",
);

/// Magic byte stored at offset 16 of a B+ tree header slot (slot 0 of
/// [`btree_header_address`]). Distinguishes a header from a string-index list
/// account, whose slot 0 is a plain u64 count (byte 16 always zero).
pub const BTREE_MAGIC: u8 = 0x42;

/// Maximum number of keys per B+ tree node (leaf or internal). A node splits when
/// an insert would take it past this.
pub const BTREE_ORDER: usize = 32;

/// Address of the *pair account* for the `(attr, value)` equality bucket.
///
/// `pair_address = keccak256("arkiv.pair" || attr || 0x00 || typeId || value)[:20]`.
/// The account's contents are the [`Bitmap`](crate::bitmap::Bitmap) of entity ids
/// carrying this pair. The `0x00` separator prevents prefix collisions between the
/// attribute and value, so `attr` must not itself contain `0x00` (the precompile
/// enforces this on the write path); the fixed-width `typeId` that follows it keeps
/// one name's differently-typed values in disjoint buckets, so `true` and the
/// `u256` `1` never share one.
pub fn pair_address(attr: &[u8], ty: AttributeType, value: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(PAIR_DOMAIN.len() + attr.len() + 2 + value.len());
    buf.extend_from_slice(PAIR_DOMAIN);
    buf.extend_from_slice(attr);
    buf.push(0x00);
    buf.push(ty.id());
    buf.extend_from_slice(value);
    Address::from_slice(&keccak256(buf).0[..ETH_ADDRESS_LEN])
}

/// Address of the bucket every live entity belongs to: the `($all, "")` pair.
///
/// Reading this one bitmap enumerates every entity, and negations (`Neq`, `Not`,
/// …) are evaluated as "everything in `$all`, minus the matching bitmap".
pub fn all_entities_bucket() -> Address {
    pair_address(
        arkiv_interfaces::entity::annotations::ALL,
        AttributeType::Str,
        b"",
    )
}

/// Address of the **header account** for the tier-2 B+ tree over `attr`'s `ty`
/// values.
///
/// `btree_header_address = keccak256("arkiv.ibth" || attr || 0x00 || typeId)[:20]`.
/// Slot 0 holds the tree's header: `[0..8]` = root node id, `[8..16]` = next node id
/// to allocate, `[16]` = [`BTREE_MAGIC`] — all big-endian. One tree per
/// `(attr, type)`, so its keys are all the same width and their byte order is the
/// type's value order.
pub fn btree_header_address(attr: &[u8], ty: AttributeType) -> Address {
    let mut buf = Vec::with_capacity(BTREE_HEADER_DOMAIN.len() + attr.len() + 2);
    buf.extend_from_slice(BTREE_HEADER_DOMAIN);
    buf.extend_from_slice(attr);
    buf.push(0x00);
    buf.push(ty.id());
    Address::from_slice(&keccak256(buf).0[..ETH_ADDRESS_LEN])
}

/// Address of the **node account** with id `node_id` under `header_addr`.
///
/// A B+ tree's nodes each get their own account; this maps `(tree, node id)` to
/// that account's address. The hash preimage is a fixed 38 bytes:
///
/// ```text
///   byte:  0 ................ 10 ...................... 30 ......... 38
///          | "arkiv.ibtn" (10) | header_addr (ETH_ADDRESS_LEN) | node_id (u64) |
/// ```
///
/// i.e. `keccak256("arkiv.ibtn" || header_addr || node_id_be)[..ETH_ADDRESS_LEN]`.
/// Folding `header_addr` into the preimage **namespaces the node ids to their own
/// tree**, so node 1 of one attribute's index and node 1 of another's never land
/// at the same account. `node_id` is big-endian; id 0 is never allocated (it is the
/// header's "no root / no node" sentinel), so no node account collides with the
/// header account.
pub fn btree_node_address(header_addr: Address, node_id: u64) -> Address {
    let mut buf = [0u8; DOMAIN_TAG_LEN + ETH_ADDRESS_LEN + size_of::<u64>()];
    buf[..DOMAIN_TAG_LEN].copy_from_slice(BTREE_NODE_DOMAIN);
    buf[DOMAIN_TAG_LEN..DOMAIN_TAG_LEN + ETH_ADDRESS_LEN].copy_from_slice(header_addr.as_slice());
    buf[DOMAIN_TAG_LEN + ETH_ADDRESS_LEN..].copy_from_slice(&node_id.to_be_bytes());
    Address::from_slice(&keccak256(buf).0[..ETH_ADDRESS_LEN])
}

/// Address of the cascade **level account** for `attr`'s `ty` values at `prefix`.
///
/// `str_level_address = keccak256("arkiv.sidx" || attr || 0x00 || typeId ||
/// prefix)[..20]`. The cascade indexes a string value chunk-by-chunk: level 0 lives
/// at `prefix = b""`, level 1 at `prefix = chunk0` (32 bytes), and so on. Each level
/// account holds one storage slot per distinct 32-byte chunk seen at that level.
/// Separator and `typeId` as in [`pair_address`].
pub fn str_level_address(attr: &[u8], ty: AttributeType, prefix: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(STR_LEVEL_DOMAIN.len() + attr.len() + 2 + prefix.len());
    buf.extend_from_slice(STR_LEVEL_DOMAIN);
    buf.extend_from_slice(attr);
    buf.push(0x00);
    buf.push(ty.id());
    buf.extend_from_slice(prefix);
    Address::from_slice(&keccak256(buf).0[..ETH_ADDRESS_LEN])
}

/// Address of the **enumeration list** companion for an index account.
///
/// `list_address_for = keccak256("arkiv.list" || index_addr)[..20]`. EVM storage
/// isn't range-scannable, so each cascade level account has a companion list that
/// records, in insertion order, every distinct slot key ever written to it — letting
/// the reader enumerate a level's live entries. Slot `0` holds the count; slot `i`
/// (1-based) holds the `i`-th key.
pub fn list_address_for(index_addr: Address) -> Address {
    let mut buf = Vec::with_capacity(LIST_DOMAIN.len() + ETH_ADDRESS_LEN);
    buf.extend_from_slice(LIST_DOMAIN);
    buf.extend_from_slice(index_addr.as_slice());
    Address::from_slice(&keccak256(buf).0[..ETH_ADDRESS_LEN])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    use AttributeType::{Str, U256};

    // ── Pinned addresses ──────────────────────────────────────────────
    //
    // The tests below pin where buckets live, not that the derivation is
    // self-consistent. **A failure here is not a broken test** — it means the
    // index address space moved, so every bucket already written on chain is
    // orphaned. That is a hard fork, and the only correct responses are to
    // revert the change or to migrate deliberately.
    //
    // They exist because nothing else catches this. Renaming a domain tag or
    // reordering the preimage leaves every structural test below passing while
    // silently relocating the whole index.
    //
    // To re-pin after an intended change, recompute with an independent keccak
    // rather than copying what the code now returns — otherwise the vector just
    // restates the bug:
    //
    //     cast keccak 0x$(printf 'arkiv.pair$all\x00\x08' | xxd -p -c 256)
    //
    // Last re-pinned when the frozen type set (`arkiv-node-api.md` §2) inserted
    // `u64` at typeId 3 and shifted every tag above it — `Str` 7→8, `U256` 3→4.
    // The typeId is part of every preimage here, so all five vectors moved.

    /// Where the `$all` marker's bucket lives — the bitmap every query with a
    /// negation reads.
    #[test]
    fn all_marker_bucket_address_is_pinned() {
        assert_eq!(
            pair_address(b"$all", Str, b""),
            address!("9ba236460280c9c91f261f4b74830f797e48ebe9"),
        );
    }

    /// Where an ordinary `(name, value)` bucket lives, covering the attribute
    /// and value halves of the preimage that `$all` (empty value) does not.
    #[test]
    fn user_attribute_bucket_address_is_pinned() {
        assert_eq!(
            pair_address(b"color", Str, b"blue"),
            address!("e338f779d1c294ce7e8ab45d9e4d163d596ee022"),
        );
    }

    #[test]
    fn all_entities_bucket_is_all_empty_pair() {
        assert_eq!(all_entities_bucket(), pair_address(b"$all", Str, b""));
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(pair_address(b"k", Str, b"v"), pair_address(b"k", Str, b"v"));
    }

    /// The `0x00` separator is what stops the `(attr, value)` split point from
    /// sliding: without it, `("ab", "c")` and `("a", "bc")` would hash the same
    /// concatenation and share a bucket.
    #[test]
    fn separator_prevents_prefix_collision() {
        assert_ne!(
            pair_address(b"ab", Str, b"c"),
            pair_address(b"a", Str, b"bc")
        );
    }

    #[test]
    fn distinct_values_get_distinct_buckets() {
        assert_ne!(
            pair_address(b"$owner", Str, b"a"),
            pair_address(b"$owner", Str, b"b")
        );
    }

    /// Same name, same bytes, different type — different bucket. Without this, a
    /// `bool` `true` and a one-byte string would answer each other's queries.
    #[test]
    fn distinct_types_get_distinct_buckets() {
        assert_ne!(
            pair_address(b"flag", AttributeType::Bool, &[1]),
            pair_address(b"flag", Str, &[1]),
        );
        assert_ne!(
            btree_header_address(b"n", AttributeType::Int),
            btree_header_address(b"n", U256),
        );
        assert_ne!(
            str_level_address(b"s", Str, b""),
            str_level_address(b"s", AttributeType::Bytes32, b""),
        );
    }

    #[test]
    fn btree_header_golden() {
        assert_eq!(
            btree_header_address(b"$expiration", U256),
            address!("f9a6d8939a54cdec14e7942c50d2c0dfe62904eb"),
        );
    }

    #[test]
    fn btree_node_golden() {
        let header = btree_header_address(b"$expiration", U256);
        assert_eq!(
            btree_node_address(header, 1),
            address!("a44efef67695d6ebf47368563619f09ec6f4b2b4"),
        );
    }

    #[test]
    fn btree_nodes_are_distinct_per_id_and_tree() {
        let a = btree_header_address(b"attrA", U256);
        let b = btree_header_address(b"attrB", U256);
        assert_ne!(btree_node_address(a, 1), btree_node_address(a, 2));
        assert_ne!(btree_node_address(a, 1), btree_node_address(b, 1));
    }

    #[test]
    fn str_level_golden() {
        assert_eq!(
            str_level_address(b"name", Str, b""),
            address!("436d37162beba53707a620bcf6a5435f950bfd6a"),
        );
    }

    #[test]
    fn list_golden() {
        assert_eq!(
            list_address_for(Address::from([0x11; 20])),
            address!("4467d6c452125d3c95ac7dbf3db5c89f125b5652"),
        );
    }

    #[test]
    fn str_levels_are_distinct_per_prefix_and_attr() {
        assert_ne!(
            str_level_address(b"a", Str, b""),
            str_level_address(b"a", Str, b"x")
        );
        assert_ne!(
            str_level_address(b"a", Str, b"x"),
            str_level_address(b"b", Str, b"x")
        );
    }
}
