//! Address derivations — the physical anchor for entity state.
//!
//! Each entity lives at its own reth account address, and the entity store keeps
//! its global bookkeeping (entity counter, per-caller nonces, id ↔ address maps)
//! at a single system account. These derivations are **consensus-relevant** — two
//! nodes must derive identical addresses or their state roots diverge. They are
//! ported (behaviour-wise) from the `arkiv-db-engine` reference, speaking the
//! spec's [`EntityAddress`](arkiv_interfaces::primitives::EntityAddress).
//!
//! Scope: **entities only**. The query index (equality "pair" accounts, range
//! indexes) is [`AuxiliaryStore`](arkiv_interfaces::state::AuxiliaryStore) data —
//! a separate concern that lives in its own crate, not here. This layout is
//! reth-specific: mapping Arkiv onto accounts/slots is the host's job, not the
//! Arkiv specification's.

use alloy_primitives::{Address, B256, keccak256};
use arkiv_interfaces::constants::ethereum::{ETH_ADDRESS_LEN, EVM_WORD_LENGTH};
use arkiv_interfaces::primitives::EntityAddress;

// The named widths must match the types this module bridges: an entity address
// is a spec word, and its prefix is an alloy address. Enforced at compile time.
const _: () = assert!(size_of::<EntityAddress>() == EVM_WORD_LENGTH);
const _: () = assert!(size_of::<Address>() == ETH_ADDRESS_LEN);

/// The storage-host account for entity-store bookkeeping — the global entity
/// counter, the per-caller nonce map, and the id ↔ address maps live here as
/// storage slots. Address `0x44…46`. No genesis allocation is required; it is
/// materialised lazily on first write.
pub const SYSTEM_ACCOUNT_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x46,
]);

/// The MPT leaf an entity is anchored at: the first [`ETH_ADDRESS_LEN`] bytes
/// of its [`EntityAddress`].
///
/// The leaf address is a pure anchor; the entity's content is committed via the
/// account's `codeHash`, not its address.
///
/// The truncation is a host adaptation, not an Arkiv property: an
/// [`EntityAddress`] is a full [`EVM_WORD_LENGTH`] bytes, and dropping 12 to fit
/// reth's account key leaves a 160-bit space. Two entity addresses sharing a
/// prefix currently collapse onto one leaf, and nothing downstream distinguishes
/// them — [`RethEntityStore`](crate::entities::store::RethEntityStore) stores by
/// leaf and reads back whatever is there (the record does carry the full
/// address, so a leaf *could* hold one entry per prefix-sharing entity; today it
/// holds exactly one).
#[inline]
pub fn entity_leaf_address(entity: EntityAddress) -> Address {
    Address::from_slice(&entity[..ETH_ADDRESS_LEN])
}

/// Storage slot on [`SYSTEM_ACCOUNT_ADDRESS`] holding `caller`'s entity-minting
/// nonce: `keccak256("nonces" || caller)`. The nonce feeds `Create` address
/// derivation (and the SDK's `nonces(address)` view), and is advanced once per
/// created entity.
pub fn nonce_slot(caller: Address) -> B256 {
    let mut buf = [0u8; 6 + ETH_ADDRESS_LEN];
    buf[..6].copy_from_slice(b"nonces");
    buf[6..].copy_from_slice(caller.as_slice());
    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_address_is_the_entity_address_prefix() {
        let entity: EntityAddress = [0xAB; 32];
        assert_eq!(entity_leaf_address(entity), Address::from([0xAB; 20]));
    }

    #[test]
    fn leaf_address_is_exactly_the_prefix() {
        // Two entity addresses that agree on the first ETH_ADDRESS_LEN bytes but
        // differ afterwards must map to the same leaf — the leaf address is the
        // prefix, nothing more.
        let mut a: EntityAddress = [9u8; 32];
        let mut b: EntityAddress = [9u8; 32];
        a[ETH_ADDRESS_LEN] = 1;
        b[ETH_ADDRESS_LEN] = 2;
        assert_eq!(entity_leaf_address(a), entity_leaf_address(b));
    }

    #[test]
    fn system_account_is_0x44_dot_dot_46() {
        let a = SYSTEM_ACCOUNT_ADDRESS.as_slice();
        assert_eq!(a[0], 0x44);
        assert_eq!(a[19], 0x46);
        assert!(a[1..19].iter().all(|b| *b == 0));
    }
}
