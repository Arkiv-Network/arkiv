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

use alloy_primitives::{Address, keccak256};

/// Domain tag for a tier-1 equality (pair) bucket.
const PAIR_DOMAIN: &[u8] = b"arkiv.pair";

/// Address of the *pair account* for the `(attr, value)` equality bucket.
///
/// `pair_address = keccak256("arkiv.pair" || attr || 0x00 || value)[:20]`. The
/// account's contents are the [`Bitmap`](crate::bitmap::Bitmap) of entity ids
/// carrying this pair. The `0x00` separator prevents prefix collisions between the
/// attribute and value, so `attr` and `value` must not themselves contain `0x00`
/// (the precompile enforces this on the write path).
pub fn pair_address(attr: &[u8], value: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(PAIR_DOMAIN.len() + attr.len() + 1 + value.len());
    buf.extend_from_slice(PAIR_DOMAIN);
    buf.extend_from_slice(attr);
    buf.push(0x00);
    buf.extend_from_slice(value);
    Address::from_slice(&keccak256(buf).0[..20])
}

/// Address of the bucket every live entity belongs to: the `($all, "")` pair.
///
/// Reading this one bitmap enumerates every entity, and negations (`Neq`, `Not`,
/// …) are evaluated as "everything in `$all`, minus the matching bitmap".
pub fn all_entities_bucket() -> Address {
    pair_address(arkiv_interfaces::entity::annotations::ALL, b"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    // Golden vectors — cross-checked against an independent keccak (`cast keccak`).
    // If either of these assertions ever changes, the on-chain index address space
    // has moved and every existing bucket is orphaned: it is a hard fork, not a
    // refactor.

    #[test]
    fn pair_address_golden_all() {
        assert_eq!(
            pair_address(b"$all", b""),
            address!("6e5ac232ad0532401f1a1b4e84410abe42fd7738"),
        );
    }

    #[test]
    fn pair_address_golden_user_attr() {
        assert_eq!(
            pair_address(b"color", b"blue"),
            address!("305e7f4a747af42bfbb1c4e8b33ea4b0526e12d2"),
        );
    }

    #[test]
    fn all_entities_bucket_is_all_empty_pair() {
        assert_eq!(all_entities_bucket(), pair_address(b"$all", b""));
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(pair_address(b"k", b"v"), pair_address(b"k", b"v"));
    }

    /// The `0x00` separator is what stops the `(attr, value)` split point from
    /// sliding: without it, `("ab", "c")` and `("a", "bc")` would hash the same
    /// concatenation and share a bucket.
    #[test]
    fn separator_prevents_prefix_collision() {
        assert_ne!(pair_address(b"ab", b"c"), pair_address(b"a", b"bc"));
    }

    #[test]
    fn distinct_values_get_distinct_buckets() {
        assert_ne!(pair_address(b"$owner", b"a"), pair_address(b"$owner", b"b"));
    }
}
