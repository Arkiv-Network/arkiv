//! The two raw account seams the reth host still owns.
//!
//! An Arkiv `UserBalance` *is* the Ethereum account balance and a `UserNonce`
//! *is* the account nonce, so these two lanes cannot simply move into GolemDB
//! while reth's state provider is what answers `eth_getBalance` and what the
//! txpool checks. Everything else Arkiv stores — entities, the query index,
//! the pruning set, the minting nonces — is GolemDB's.
//!
//! Keeping them as traits rather than calling revm directly means
//! [`HostStateView`](crate::HostStateView) is testable against an in-memory
//! map; [`WriteOverlay`](crate::WriteOverlay) implements both over its
//! `EvmState` diff.
//!
//! These disappear when the GolemDB state provider lands and reth reads
//! balances from the store's own `bal` and `non` cells.

use alloy_primitives::{Address, U256};

/// An Ethereum account's **balance** field.
pub trait BalanceAccess {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's balance; zero if the account doesn't exist.
    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error>;

    /// Set the account's balance, creating the account if needed.
    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error>;
}

/// An Ethereum account's **nonce** field.
pub trait NonceAccess {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's transaction nonce; zero if the account doesn't exist.
    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error>;

    /// Set the account's transaction nonce.
    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error>;
}

/// Forwarding impls, so a view that lends out a `&mut` to its backend still
/// satisfies the seams.
impl<T: BalanceAccess + ?Sized> BalanceAccess for &mut T {
    type Error = T::Error;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
        (**self).get_balance(addr)
    }

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
        (**self).set_balance(addr, balance)
    }
}

impl<T: NonceAccess + ?Sized> NonceAccess for &mut T {
    type Error = T::Error;

    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error> {
        (**self).get_nonce(addr)
    }

    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error> {
        (**self).set_nonce(addr, nonce)
    }
}
