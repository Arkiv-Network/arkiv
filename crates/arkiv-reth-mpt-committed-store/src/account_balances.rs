//! [`RethAccountBalancesStore`] — raw balance reads and writes.
//!
//! The store's own job is one conversion: the spec speaks in [`UserBalance`]
//! big-endian bytes, the host in alloy [`U256`]s. Where a balance physically
//! lives is the [`BalanceAccess`] seam's concern — on this host, in the
//! Ethereum account itself. Keeping the seam a trait means the store logic is
//! testable against an in-memory map; the reth write overlay implements it over
//! its `EvmState` diff.

use alloy_primitives::{Address, U256};

use arkiv_interfaces::primitives::{UserAddress, UserBalance};

/// This store's raw seam: an Ethereum account's **balance** field.
pub trait BalanceAccess {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's balance; zero if the account doesn't exist.
    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error>;

    /// Set the account's balance, creating the account if needed.
    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error>;
}

/// Forwarding impl, so the store (which takes its backend by value) can be
/// built over a *borrowed* backend too — e.g. a state manager lending out its
/// one underlying state for the duration of a call.
impl<T: BalanceAccess + ?Sized> BalanceAccess for &mut T {
    type Error = T::Error;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
        (**self).get_balance(addr)
    }

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
        (**self).set_balance(addr, balance)
    }
}

/// The reth-host balance store: balances live in the accounts, reached through
/// a [`BalanceAccess`].
#[derive(Debug, Default, Clone)]
pub struct RethAccountBalancesStore<B> {
    backend: B,
}

impl<B> RethAccountBalancesStore<B> {
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

impl<B: BalanceAccess> RethAccountBalancesStore<B> {
    pub fn get_balance(&mut self, account: UserAddress) -> Result<UserBalance, B::Error> {
        let balance = self.backend.get_balance(Address::from(account))?;
        Ok(UserBalance::from_be_bytes(balance.to_be_bytes()))
    }

    pub fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), B::Error> {
        self.backend.set_balance(
            Address::from(account),
            U256::from_be_bytes(balance.to_be_bytes()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An in-memory [`BalanceAccess`]: address → balance, absent reads as zero.
    #[derive(Debug, Default)]
    struct MemBalances {
        balances: HashMap<Address, U256>,
    }

    impl BalanceAccess for MemBalances {
        type Error = core::convert::Infallible;

        fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
            Ok(self.balances.get(&addr).copied().unwrap_or_default())
        }

        fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
            self.balances.insert(addr, balance);
            Ok(())
        }
    }

    #[test]
    fn balances_round_trip_the_spec_boundary() {
        let mut store = RethAccountBalancesStore::new(MemBalances::default());
        let alice: UserAddress = [0xAA; 20];

        // Absent reads as zero, not as an error.
        assert_eq!(store.get_balance(alice).unwrap(), UserBalance::ZERO);

        store
            .set_balance(alice, UserBalance::from_u64(1_000))
            .unwrap();
        assert_eq!(
            store.get_balance(alice).unwrap(),
            UserBalance::from_u64(1_000)
        );

        // The backend holds the alloy-typed value at the alloy-typed address.
        assert_eq!(
            store.backend().balances[&Address::from(alice)],
            U256::from(1_000u64)
        );
    }

    /// The conversion is byte-exact across the full width, not just u64-range.
    #[test]
    fn wide_balances_survive_the_conversion() {
        let mut store = RethAccountBalancesStore::new(MemBalances::default());
        let alice: UserAddress = [0xAA; 20];
        let wide = UserBalance::from_be_bytes([0xAB; 32]);

        store.set_balance(alice, wide).unwrap();
        assert_eq!(store.get_balance(alice).unwrap(), wide);
    }
}
