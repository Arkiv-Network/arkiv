//! The two seams the account mirror writes through.
//!
//! Balances and nonces live in GolemDB. These traits are the *write* half of
//! the mirror that copies them into the `EvmState` diff reth's block executor
//! commits — see [`host`](crate::host) for why the diff is still needed.
//!
//! Nothing reads through them. They are traits rather than direct revm calls
//! so [`HostStateView`](crate::HostStateView) is testable against an in-memory
//! map; [`WriteOverlay`](crate::WriteOverlay) is the real implementation, over
//! its diff.

use alloy_primitives::{Address, U256};

/// An Ethereum account's **balance** field, as reth's diff holds it.
pub trait BalanceAccess {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's balance in the diff; zero if it holds no entry for it.
    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error>;

    /// Set the account's balance, creating the account if needed.
    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error>;
}

/// An Ethereum account's **nonce** field, as reth's diff holds it.
pub trait NonceAccess {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's nonce in the diff; zero if it holds no entry for it.
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
