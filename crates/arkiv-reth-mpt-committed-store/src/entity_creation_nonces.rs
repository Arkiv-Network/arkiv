//! [`RethEntityCreationNoncesStore`] — raw entity-minting-nonce
//! reads and writes.
//!
//! On this host an owner's entity-minting nonce lives as a storage slot on the
//! system account ([`nonce_slot`] on [`SYSTEM_ACCOUNT_ADDRESS`]) — the same
//! slot the SDK's `entityNonce(address)` view answers from, so predicted and
//! minted keys agree by construction. The store runs over the storage-slot seam
//! ([`IndexStorage`] — named for its main consumer, the index, but it is just
//! "an account's storage slots").

use alloy_primitives::{Address, B256, U256};

use crate::entities::layout::{SYSTEM_ACCOUNT_ADDRESS, nonce_slot};
use crate::indices::IndexStorage;
use arkiv_interfaces::primitives::{EntityCreationNonce, UserAddress};

/// The reth-host entity-minting-nonce store: minting nonces are
/// system-account storage slots, reached through an [`IndexStorage`].
#[derive(Debug, Default, Clone)]
pub struct RethEntityCreationNoncesStore<B> {
    backend: B,
}

impl<B> RethEntityCreationNoncesStore<B> {
    /// Wrap a backend.
    pub const fn new(backend: B) -> Self {
        Self { backend }
    }

    /// The underlying backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Unwrap the backend.
    pub fn into_backend(self) -> B {
        self.backend
    }
}

impl<B: IndexStorage> RethEntityCreationNoncesStore<B> {
    pub fn get_entity_nonce(
        &mut self,
        owner: UserAddress,
    ) -> Result<EntityCreationNonce, B::Error> {
        let word = self
            .backend
            .storage(SYSTEM_ACCOUNT_ADDRESS, nonce_slot(Address::from(owner)))?;
        Ok(EntityCreationNonce::new(
            U256::from_be_bytes(word.0).saturating_to::<u64>(),
        ))
    }

    /// Returns the nonce **before** the advance.
    pub fn advance_entity_nonce(
        &mut self,
        owner: UserAddress,
        by: u64,
    ) -> Result<EntityCreationNonce, B::Error> {
        let current = self.get_entity_nonce(owner)?;
        self.set_entity_nonce(owner, current.advanced_by(by))?;
        Ok(current)
    }

    pub fn set_entity_nonce(
        &mut self,
        owner: UserAddress,
        nonce: EntityCreationNonce,
    ) -> Result<(), B::Error> {
        // Materialise the system account on its first write, or EIP-161 prunes
        // it (and the nonce with it) at end of block.
        self.backend
            .ensure_account_persists(SYSTEM_ACCOUNT_ADDRESS)?;
        self.backend.set_storage(
            SYSTEM_ACCOUNT_ADDRESS,
            nonce_slot(Address::from(owner)),
            B256::from(U256::from(nonce.get()).to_be_bytes()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// An in-memory storage-slot seam: `(addr, slot) → value`, plus the set of
    /// accounts kept alive — so the EIP-161 guard is observable.
    #[derive(Debug, Default)]
    struct MemSlots {
        slots: HashMap<(Address, B256), B256>,
        persisted: HashSet<Address>,
    }

    impl IndexStorage for MemSlots {
        type Error = core::convert::Infallible;

        fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Self::Error> {
            Ok(self.slots.get(&(addr, slot)).copied().unwrap_or(B256::ZERO))
        }

        fn set_storage(
            &mut self,
            addr: Address,
            slot: B256,
            value: B256,
        ) -> Result<(), Self::Error> {
            self.slots.insert((addr, slot), value);
            Ok(())
        }

        fn ensure_account_persists(&mut self, addr: Address) -> Result<(), Self::Error> {
            self.persisted.insert(addr);
            Ok(())
        }
    }

    #[test]
    fn advance_returns_the_pre_advance_nonce() {
        let mut store = RethEntityCreationNoncesStore::new(MemSlots::default());
        let alice: UserAddress = [0xAA; 20];

        assert_eq!(
            store.get_entity_nonce(alice).unwrap(),
            EntityCreationNonce::ZERO
        );
        // A batch of two creates: advance returns the start (0), leaves 2.
        assert_eq!(
            store.advance_entity_nonce(alice, 2).unwrap(),
            EntityCreationNonce::ZERO
        );
        assert_eq!(
            store.get_entity_nonce(alice).unwrap(),
            EntityCreationNonce::new(2)
        );
        // Next batch of one: start 2, leaves 3.
        assert_eq!(
            store.advance_entity_nonce(alice, 1).unwrap(),
            EntityCreationNonce::new(2)
        );
        assert_eq!(
            store.get_entity_nonce(alice).unwrap(),
            EntityCreationNonce::new(3)
        );
    }

    #[test]
    fn nonces_are_per_owner() {
        let mut store = RethEntityCreationNoncesStore::new(MemSlots::default());
        store.advance_entity_nonce([0xAA; 20], 5).unwrap();
        assert_eq!(
            store.get_entity_nonce([0xAA; 20]).unwrap(),
            EntityCreationNonce::new(5)
        );
        assert_eq!(
            store.get_entity_nonce([0xBB; 20]).unwrap(),
            EntityCreationNonce::ZERO
        );
    }

    /// The nonce lands in the documented slot on the system account, which is
    /// kept alive against EIP-161.
    #[test]
    fn advance_writes_the_system_account_slot() {
        let mut store = RethEntityCreationNoncesStore::new(MemSlots::default());
        let alice: UserAddress = [0xAA; 20];
        store.advance_entity_nonce(alice, 1).unwrap();

        let backend = store.into_backend();
        assert!(backend.persisted.contains(&SYSTEM_ACCOUNT_ADDRESS));
        assert_eq!(
            backend.slots[&(SYSTEM_ACCOUNT_ADDRESS, nonce_slot(Address::from(alice)))],
            B256::from(U256::from(1u64).to_be_bytes()),
        );
    }
}
