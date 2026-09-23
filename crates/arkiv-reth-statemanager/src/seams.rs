//! The raw seams a base must offer: the two Ethereum account fields Arkiv
//! touches, and the anchor slot holding the database root. One backend
//! implementing all three, the write overlay or an in-memory mock, carries
//! every Ethereum-side store at once.

use alloy_primitives::{Address, B256, U256};
use arkiv_interfaces::primitives::{UserAddress, UserBalance, UserNonce};

pub trait BalanceAccess {
    type Error: core::fmt::Debug;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error>;

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error>;
}

impl<T: BalanceAccess + ?Sized> BalanceAccess for &mut T {
    type Error = T::Error;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
        (**self).get_balance(addr)
    }

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
        (**self).set_balance(addr, balance)
    }
}

pub trait NonceAccess {
    type Error: core::fmt::Debug;

    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error>;

    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error>;
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

/// The database root, as held by the anchor account's slot.
pub trait AnchorAccess {
    type Error: core::fmt::Debug;

    fn db_root(&mut self) -> Result<B256, Self::Error>;

    fn set_db_root(&mut self, root: B256) -> Result<(), Self::Error>;
}

/// The balances store: the spec's types over the [`BalanceAccess`] seam.
pub struct RethAccountBalancesStore<B>(pub B);

impl<B: BalanceAccess> RethAccountBalancesStore<B> {
    pub fn get_balance(&mut self, account: UserAddress) -> Result<UserBalance, B::Error> {
        let balance = self.0.get_balance(Address::from(account))?;
        Ok(UserBalance::from_be_bytes(balance.to_be_bytes()))
    }

    pub fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), B::Error> {
        self.0.set_balance(
            Address::from(account),
            U256::from_be_bytes(balance.to_be_bytes()),
        )
    }
}

/// The transaction nonces store over the [`NonceAccess`] seam.
pub struct RethAccountNoncesStore<B>(pub B);

impl<B: NonceAccess> RethAccountNoncesStore<B> {
    pub fn get_account_nonce(&mut self, account: UserAddress) -> Result<UserNonce, B::Error> {
        self.0.get_nonce(Address::from(account)).map(UserNonce::new)
    }

    pub fn set_account_nonce(
        &mut self,
        account: UserAddress,
        nonce: UserNonce,
    ) -> Result<(), B::Error> {
        self.0.set_nonce(Address::from(account), nonce.get())
    }
}
