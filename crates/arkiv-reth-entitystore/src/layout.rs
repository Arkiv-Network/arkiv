//! Address derivations — the physical anchor for entity state.
//!
//! Each entity lives at its own reth account address, and the entity store keeps
//! its global bookkeeping (entity counter, per-caller nonces, id ↔ address maps)
//! at a single system account. These derivations are **consensus-relevant** — two
//! nodes must derive identical addresses or their state roots diverge. They are
//! ported (behaviour-wise) from the `arkiv-db-engine` reference; only the
//! entity-key type is refactored to the spec's
//! [`EntityKey`](arkiv_interfaces::primitives::EntityKey).
//!
//! Scope: **entities only**. The query index (equality "pair" accounts, range
//! indexes) is [`AuxiliaryStore`](arkiv_interfaces::state::AuxiliaryStore) data —
//! a separate concern that lives in its own crate, not here. This layout is
//! reth-specific: mapping Arkiv onto accounts/slots is the host's job, not the
//! Arkiv specification's.

use alloy_primitives::{Address, B256, keccak256};
use arkiv_interfaces::constants::{ADDRESS_LEN, WORD_LEN};
use arkiv_interfaces::primitives::EntityKey;

// The named widths must match the types this module bridges: an entity key is a
// spec word, and its prefix is an alloy address. Enforced at compile time.
const _: () = assert!(size_of::<EntityKey>() == WORD_LEN);
const _: () = assert!(size_of::<Address>() == ADDRESS_LEN);

/// The storage-host account for entity-store bookkeeping — the global entity
/// counter, the per-caller nonce map, and the id ↔ address maps live here as
/// storage slots. Address `0x44…46`. No genesis allocation is required; it is
/// materialised lazily on first write.
pub const SYSTEM_ACCOUNT_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x46,
]);

/// Entity-account address: the first [`ADDRESS_LEN`] bytes of the entity key.
///
/// The address is a pure identity anchor; the entity's content is committed via
/// the account's `codeHash`, not its address.
#[inline]
pub fn entity_address(key: EntityKey) -> Address {
    Address::from_slice(&key[..ADDRESS_LEN])
}

/// Storage slot on [`SYSTEM_ACCOUNT_ADDRESS`] holding `caller`'s entity-key minting
/// nonce: `keccak256("nonces" || caller)`. The nonce feeds `Create` key derivation
/// (and the SDK's `nonces(address)` view), and is advanced once per created entity.
pub fn nonce_slot(caller: Address) -> B256 {
    let mut buf = [0u8; 6 + ADDRESS_LEN];
    buf[..6].copy_from_slice(b"nonces");
    buf[6..].copy_from_slice(caller.as_slice());
    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_address_is_the_key_prefix() {
        let key: EntityKey = [0xAB; 32];
        assert_eq!(entity_address(key), Address::from([0xAB; 20]));
    }

    #[test]
    fn address_is_exactly_the_key_prefix() {
        // Two keys that agree on the first ADDRESS_LEN bytes but differ afterwards
        // must map to the same account — the address is the prefix, nothing more.
        let mut a: EntityKey = [9u8; 32];
        let mut b: EntityKey = [9u8; 32];
        a[ADDRESS_LEN] = 1;
        b[ADDRESS_LEN] = 2;
        assert_eq!(entity_address(a), entity_address(b));
    }

    #[test]
    fn distinct_keys_give_distinct_addresses() {
        let mut a: EntityKey = [0u8; 32];
        let mut b: EntityKey = [0u8; 32];
        a[0] = 1;
        b[0] = 2;
        assert_ne!(entity_address(a), entity_address(b));
    }

    #[test]
    fn system_account_is_0x44_dot_dot_46() {
        let a = SYSTEM_ACCOUNT_ADDRESS.as_slice();
        assert_eq!(a[0], 0x44);
        assert_eq!(a[19], 0x46);
        assert!(a[1..19].iter().all(|b| *b == 0));
    }
}
