//! [`RethAccountNoncesStore`] — raw transaction-nonce reads and
//! writes.
//!
//! The **transaction** nonce (replay protection), not the entity-minting one —
//! that is [`RethEntityCreationNoncesStore`](crate::RethEntityCreationNoncesStore),
//! and the two must never mix (see `EntityCreationNonce`'s docs in `arkiv_interfaces`).
//! On this host the transaction nonce is the Ethereum account's own nonce
//! field, reached through the [`NonceAccess`] seam.

use alloy_primitives::Address;

use arkiv_interfaces::primitives::{UserAddress, UserNonce};

/// This store's raw seam: an Ethereum account's **nonce** field.
pub trait NonceAccess {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's transaction nonce; zero if the account doesn't exist.
    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error>;

    /// Set the account's transaction nonce.
    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error>;
}

/// Forwarding impl, so the store (which takes its backend by value) can be
/// built over a *borrowed* backend too — e.g. a state manager lending out its
/// one underlying state for the duration of a call.
impl<T: NonceAccess + ?Sized> NonceAccess for &mut T {
    type Error = T::Error;

    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error> {
        (**self).get_nonce(addr)
    }

    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error> {
        (**self).set_nonce(addr, nonce)
    }
}

/// The reth-host transaction-nonce store: transaction nonces are the accounts'
/// own nonce fields, reached through a [`NonceAccess`].
#[derive(Debug, Default, Clone)]
pub struct RethAccountNoncesStore<B> {
    backend: B,
}

impl<B> RethAccountNoncesStore<B> {
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

impl<B: NonceAccess> RethAccountNoncesStore<B> {
    pub fn get_account_nonce(&mut self, account: UserAddress) -> Result<UserNonce, B::Error> {
        self.backend
            .get_nonce(Address::from(account))
            .map(UserNonce::new)
    }

    pub fn set_account_nonce(
        &mut self,
        account: UserAddress,
        nonce: UserNonce,
    ) -> Result<(), B::Error> {
        self.backend.set_nonce(Address::from(account), nonce.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An in-memory [`NonceAccess`]: address → nonce, absent reads as zero.
    #[derive(Debug, Default)]
    struct MemNonces {
        nonces: HashMap<Address, u64>,
    }

    impl NonceAccess for MemNonces {
        type Error = core::convert::Infallible;

        fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error> {
            Ok(self.nonces.get(&addr).copied().unwrap_or_default())
        }

        fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error> {
            self.nonces.insert(addr, nonce);
            Ok(())
        }
    }

    #[test]
    fn nonces_read_zero_then_stick() {
        let mut store = RethAccountNoncesStore::new(MemNonces::default());
        let alice: UserAddress = [0xAA; 20];
        let bob: UserAddress = [0xBB; 20];

        assert_eq!(store.get_account_nonce(alice).unwrap(), UserNonce::ZERO);
        store.set_account_nonce(alice, UserNonce::new(7)).unwrap();
        assert_eq!(store.get_account_nonce(alice).unwrap(), UserNonce::new(7));
        // Per-account, not global.
        assert_eq!(store.get_account_nonce(bob).unwrap(), UserNonce::ZERO);
    }
}
