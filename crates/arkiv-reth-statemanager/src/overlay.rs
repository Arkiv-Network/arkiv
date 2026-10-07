//! [`WriteOverlay`] — the reth **write-path** bridge for the two account lanes.
//!
//! The no-EVM executor has no revm `Journal`: it reads accounts through the
//! revm [`Database`] trait and *returns* an [`EvmState`] diff that reth's block
//! executor commits. `WriteOverlay` is that model as a state handle — base
//! reads fall through to the `Database`, writes accumulate into an in-flight
//! `EvmState` overlay (with read-your-own-writes), and
//! [`into_state`](WriteOverlay::into_state) hands the diff back for the
//! `ResultAndState`.
//!
//! It carries the balance store's [`BalanceAccess`] and the nonce store's
//! [`NonceAccess`] — the two fields of an Ethereum account that are also
//! Arkiv's, and the only state the reth side still holds. Entities, the query
//! index and the minting nonces are GolemDB's and never touch this diff.
//!
//! [`Database`]: reth_ethereum::evm::primitives::Database

use alloy_primitives::{Address, U256};

use crate::accounts::{BalanceAccess, NonceAccess};
use reth_ethereum::evm::{
    primitives::Database,
    revm::state::{Account, AccountInfo, EvmState},
};

/// A read-through / write-accumulate view of account state for one transaction:
/// reads consult the pending diff then the base [`Database`]; writes land in the
/// diff. Consume it with [`into_state`](WriteOverlay::into_state) to get the
/// `EvmState` for the transaction's `ResultAndState`.
pub struct WriteOverlay<'a, DB: Database> {
    db: &'a mut DB,
    state: EvmState,
}

impl<'a, DB: Database> WriteOverlay<'a, DB> {
    /// Start an empty diff over `db`.
    pub fn new(db: &'a mut DB) -> Self {
        Self {
            db,
            state: EvmState::default(),
        }
    }

    /// The accumulated account diff, to return in a `ResultAndState`.
    pub fn into_state(self) -> EvmState {
        self.state
    }

    /// The account's current info: the pending diff if it's been touched, else the
    /// base `Database` (default if the account is absent).
    fn load_info(&mut self, addr: Address) -> Result<AccountInfo, eyre::Report> {
        if let Some(acc) = self.state.get(&addr) {
            return Ok(acc.info.clone());
        }
        Ok(self
            .db
            .basic(addr)
            .map_err(|e| eyre::eyre!("db.basic({addr}): {e:?}"))?
            .unwrap_or_default())
    }

    /// Stage `info` as a touched account in the diff, preserving any storage the
    /// account already has staged.
    fn stage(&mut self, addr: Address, info: AccountInfo) {
        match self.state.get_mut(&addr) {
            Some(acc) => {
                acc.info = info;
                acc.mark_touch();
            }
            None => {
                let mut acc = Account::from(info);
                acc.mark_touch();
                self.state.insert(addr, acc);
            }
        }
    }

}

impl<DB: Database> BalanceAccess for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
        Ok(self.load_info(addr)?.balance)
    }

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
        let mut info = self.load_info(addr)?;
        info.balance = balance;
        self.stage(addr, info);
        Ok(())
    }
}

/// The nonce seam, with the same staging conventions as [`BalanceAccess`]
/// above.
impl<DB: Database> NonceAccess for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error> {
        Ok(self.load_info(addr)?.nonce)
    }

    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error> {
        let mut info = self.load_info(addr)?;
        info.nonce = nonce;
        self.stage(addr, info);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use reth_ethereum::evm::revm::database_interface::EmptyDBTyped;

    /// The two lanes reth still owns stage into the one diff the block
    /// executor commits.
    #[test]
    fn account_seams_stage_balance_and_nonce() {
        let mut db = EmptyDBTyped::<Infallible>::default();
        let mut overlay = WriteOverlay::new(&mut db);
        let addr = Address::repeat_byte(0xaa);

        assert_eq!(overlay.get_balance(addr).unwrap(), U256::ZERO);
        assert_eq!(overlay.get_nonce(addr).unwrap(), 0);

        overlay.set_balance(addr, U256::from(500)).unwrap();
        overlay.set_nonce(addr, 7).unwrap();

        assert_eq!(overlay.get_balance(addr).unwrap(), U256::from(500));
        assert_eq!(overlay.get_nonce(addr).unwrap(), 7);

        let state = overlay.into_state();
        let account = state.get(&addr).expect("the account is in the diff");
        assert_eq!(account.info.balance, U256::from(500));
        assert_eq!(account.info.nonce, 7);
    }
}
