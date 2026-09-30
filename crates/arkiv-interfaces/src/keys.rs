//! The store's reserved key space: which records exist, and how their keys are built.
//!
//! # Why addresses cannot simply be keys
//!
//! Every address is 32 bytes and so is every [`RecordKey`](crate::store::RecordKey),
//! so account keys and entity keys occupy the *same* space. Nothing structural keeps
//! them apart — an account address and an entity address can be the same 32 bytes,
//! and then one record is two things.
//!
//! (A narrower, 20-byte account address could be left-padded into the key space and
//! told apart by its twelve leading zeros, which is what
//! [`RecordKey::from_address`](crate::store::RecordKey::from_address) does. That
//! reservation does not survive addresses being a full 32 bytes wide, so it is not
//! what this module uses.)
//!
//! # Domain separation
//!
//! Each kind of record hashes a **domain-tagged preimage** instead. Two kinds collide
//! only if keccak collides, which is a broken hash rather than a layout mistake.
//!
//! An entity is the exception: its address is already a derived 32-byte value and is
//! used as the key directly, so entity records keep costing no hash at all. The
//! domains below are therefore all *non*-entity records, and each is tagged with a
//! string that will never be a plausible entity-address preimage.
//!
//! # Why the preimage is here and the hash is not
//!
//! These bytes reach the state root through `branch_digest`, so two nodes building
//! them differently split the chain — the layout is specification. The hash itself is
//! not: keccak is keccak. This crate carries **zero external dependencies** by
//! design, so it defines the preimage and the backend applies the hash.

use alloc::vec::Vec;

/// Domain tag for an account record: balance and the two nonces.
pub const DOMAIN_ACCOUNT: &[u8] = b"arkiv/account/v1";

/// The preimage a backend hashes to get an account record's key.
///
/// `address` is the account's full address, whatever width the host uses for one.
/// The tag is length-free because it is a fixed, unique string and the address
/// follows to the end — no other domain in this module is a prefix of it.
pub fn account_key_preimage(address: &[u8]) -> Vec<u8> {
    let mut preimage = Vec::with_capacity(DOMAIN_ACCOUNT.len() + address.len());
    preimage.extend_from_slice(DOMAIN_ACCOUNT);
    preimage.extend_from_slice(address);
    preimage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preimage_is_the_tag_then_the_address() {
        let preimage = account_key_preimage(&[0xab; 32]);
        assert_eq!(&preimage[..DOMAIN_ACCOUNT.len()], DOMAIN_ACCOUNT);
        assert_eq!(&preimage[DOMAIN_ACCOUNT.len()..], &[0xab; 32]);
    }

    #[test]
    fn different_addresses_give_different_preimages() {
        assert_ne!(
            account_key_preimage(&[1; 32]),
            account_key_preimage(&[2; 32])
        );
    }

    #[test]
    fn an_account_preimage_is_not_a_bare_address() {
        // The whole point: hashing a tagged preimage is what keeps an account key
        // out of the entity key space, where a bare address would sit.
        let address = [0x37; 32];
        assert_ne!(account_key_preimage(&address), address.to_vec());
    }

    #[test]
    fn the_tag_is_versioned() {
        // Changing the layout must change the tag, or old and new records share keys
        // and the store silently mixes two schemas.
        assert!(DOMAIN_ACCOUNT.ends_with(b"/v1"));
    }
}
