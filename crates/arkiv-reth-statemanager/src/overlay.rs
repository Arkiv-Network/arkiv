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
//! It implements the three raw seams over that one overlay: the balance
//! store's [`BalanceAccess`] and the nonce store's [`NonceAccess`] (the
//! account fields), and [`AnchorAccess`] (the database root in the anchor
//! account's slot). Entities and the index never touch Ethereum state here:
//! they live in the Arkiv database, and only their root lands in the diff.
//!
//! [`Database`]: reth_ethereum::evm::primitives::Database

use alloy_primitives::{Address, B256, U256};
use arkiv_store::{ARKIV_ROOT_ACCOUNT, ARKIV_ROOT_SLOT};
use reth_ethereum::evm::{
    primitives::Database,
    revm::state::{Account, AccountInfo, EvmState, EvmStorageSlot, TransactionId},
};

use crate::seams::{AnchorAccess, BalanceAccess, NonceAccess};

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

/// The anchor seam: the database root lives in slot 0 of the anchor account,
/// which is kept alive (nonce 1) so EIP-161 never prunes it.
impl<DB: Database> AnchorAccess for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn db_root(&mut self) -> Result<B256, Self::Error> {
        let value = self.read_slot(ARKIV_ROOT_ACCOUNT, ARKIV_ROOT_SLOT)?;
        Ok(B256::from(value.to_be_bytes::<32>()))
    }

    fn set_db_root(&mut self, root: B256) -> Result<(), Self::Error> {
        self.write_slot(
            ARKIV_ROOT_ACCOUNT,
            ARKIV_ROOT_SLOT,
            U256::from_be_bytes(root.0),
        )?;
        self.persist_account(ARKIV_ROOT_ACCOUNT)
    }
}

/// The balance seam over the same overlay: sender accounting (the gas charge)
/// and plain transfers stage into the one diff alongside the entities and the
/// index. Writes stage the account touched, *without* an EIP-161 keep-alive —
/// a plain account emptied of funds must stay prunable.
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
    use reth_ethereum::evm::revm::database_interface::EmptyDB;

    fn addr() -> Address {
        Address::from([0x11; 20])
    }

    #[test]
    fn storage_reads_and_writes_through_the_overlay() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        let slot = U256::from(7);
        assert_eq!(state.read_slot(addr(), slot).unwrap(), U256::ZERO);
        state.write_slot(addr(), slot, U256::from(42)).unwrap();
        assert_eq!(state.read_slot(addr(), slot).unwrap(), U256::from(42));
    }

    /// The anchor seam writes the root into the anchor account's slot and
    /// keeps the account alive.
    #[test]
    fn anchor_seam_stages_the_root() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        assert_eq!(state.db_root().unwrap(), B256::ZERO);
        let root = B256::repeat_byte(0xAB);
        state.set_db_root(root).unwrap();
        assert_eq!(state.db_root().unwrap(), root);
        let diff = state.into_state();
        let acc = diff
            .get(&ARKIV_ROOT_ACCOUNT)
            .expect("anchor account staged");
        assert!(acc.is_touched());
        assert_eq!(acc.info.nonce, 1);
        assert_eq!(
            acc.storage.get(&ARKIV_ROOT_SLOT).unwrap().present_value,
            U256::from_be_bytes(root.0)
        );
    }

    /// The balance and nonce seams stage balance and nonce into the same diff, and
    /// deliberately without an EIP-161 keep-alive — a plain account must stay
    /// prunable.
    #[test]
    fn account_seams_stage_balance_and_nonce() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        let a = addr();

        assert_eq!(state.get_balance(a).unwrap(), U256::ZERO);
        assert_eq!(state.get_nonce(a).unwrap(), 0);

        state.set_balance(a, U256::from(42)).unwrap();
        state.set_nonce(a, 7).unwrap();
        // Read-your-own-writes through the overlay.
        assert_eq!(state.get_balance(a).unwrap(), U256::from(42));
        assert_eq!(state.get_nonce(a).unwrap(), 7);

        let diff = state.into_state();
        let acc = diff.get(&a).expect("account staged in the diff");
        assert!(acc.is_touched());
        assert_eq!(acc.info.balance, U256::from(42));
        assert_eq!(acc.info.nonce, 7);
    }
}
