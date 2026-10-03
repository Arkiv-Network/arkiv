//! [`WriteOverlay`] — the reth **write-path** bridge, and the base a
//! [`MptStateView`](crate::MptStateView) wraps on that path.
//!
//! The no-EVM executor has no revm `Journal`: it reads accounts through the
//! revm [`Database`] trait and *returns* an [`EvmState`] diff that reth's block
//! executor commits. `WriteOverlay` is that model as a state handle — base
//! reads fall through to the `Database`, writes accumulate into an in-flight
//! `EvmState` overlay (with read-your-own-writes), and
//! [`into_state`](WriteOverlay::into_state) hands the diff back for the
//! `ResultAndState`.
//!
//! It implements **every** raw store seam over that one overlay: the entity
//! store's [`AccountCode`] (an account's code), the index's [`IndexStorage`]
//! (its storage slots), the balance store's [`BalanceAccess`] and the nonce
//! store's [`NonceAccess`] (the account's two remaining fields). Every store
//! therefore stages into the same diff — which is the point of
//! [`write_manager`](crate::write_manager): one `into_state` hands reth a
//! transaction's entities, index writes, and sender accounting together.
//!
//! [`Database`]: reth_ethereum::evm::primitives::Database

use alloy_primitives::{Address, U256};

use crate::accounts::{BalanceAccess, NonceAccess};
use reth_ethereum::evm::{
    primitives::Database,
    revm::state::{Account, AccountInfo, EvmState, EvmStorageSlot, TransactionId},
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

    /// The account in the diff, loading it (untouched) from the base `Database`
    /// first if it isn't there yet.
    fn account_mut(&mut self, addr: Address) -> Result<&mut Account, eyre::Report> {
        if !self.state.contains_key(&addr) {
            let info = self.load_info(addr)?;
            self.state.insert(addr, Account::from(info));
        }
        Ok(self.state.get_mut(&addr).expect("just inserted"))
    }

    /// A storage slot's value: the pending diff if it's been written, else the base
    /// `Database` (zero if never written). The `U256` counterpart of the
    /// [`IndexStorage::storage`] seam.
    pub fn read_slot(&mut self, addr: Address, slot: U256) -> Result<U256, eyre::Report> {
        if let Some(s) = self.state.get(&addr).and_then(|acc| acc.storage.get(&slot)) {
            return Ok(s.present_value);
        }
        self.db
            .storage(addr, slot)
            .map_err(|e| eyre::eyre!("db.storage({addr}): {e:?}"))
    }

    /// Write a storage slot into the diff. The `U256` counterpart of the
    /// [`IndexStorage::set_storage`] seam.
    pub fn write_slot(
        &mut self,
        addr: Address,
        slot: U256,
        value: U256,
    ) -> Result<(), eyre::Report> {
        // Keep the committed original across repeated writes so the diff reverts
        // correctly; only the present value changes.
        let existing_original = self
            .state
            .get(&addr)
            .and_then(|a| a.storage.get(&slot))
            .map(|s| s.original_value);
        let original = match existing_original {
            Some(o) => o,
            None => self
                .db
                .storage(addr, slot)
                .map_err(|e| eyre::eyre!("db.storage({addr}): {e:?}"))?,
        };
        let acc = self.account_mut(addr)?;
        acc.storage.insert(
            slot,
            EvmStorageSlot::new_changed(original, value, TransactionId::ZERO),
        );
        acc.mark_touch();
        Ok(())
    }

    /// Keep `addr` alive against EIP-161 pruning (raise its nonce to ≥ 1). Used to
    /// materialise the system account on its first storage write. The counterpart of
    /// the [`IndexStorage::ensure_account_persists`] seam.
    pub fn persist_account(&mut self, addr: Address) -> Result<(), eyre::Report> {
        let acc = self.account_mut(addr)?;
        if acc.info.nonce == 0 {
            acc.info.nonce = 1;
        }
        acc.mark_touch();
        Ok(())
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
