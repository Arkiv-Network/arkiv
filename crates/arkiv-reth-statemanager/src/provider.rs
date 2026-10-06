//! Reading Ethereum accounts out of the GolemDB store.
//!
//! This is the read half of moving balances and nonces off reth (#145). Today
//! [`HostStateView`](crate::HostStateView) keeps those two lanes in reth's
//! account trie because reth's state provider is what answers `eth_getBalance`
//! and what the txpool checks. This type is what will answer those instead.
//!
//! **Nothing is wired to it yet.** It is implemented and tested against the
//! store on its own, so the cutover is one change of caller rather than a new
//! implementation and a cutover at once.
//!
//! # What it does not do
//!
//! Only the account-shaped reads: balance, nonce, code, contract storage.
//! Block hashes stay with reth's own provider — headers are reth's and are not
//! part of this move — so a composed `StateProvider` delegates
//! `BlockHashReader` rather than asking the store, which has no block-number to
//! block-hash mapping on the seam anyway.
//!
//! Nothing here touches the trie-shaped traits (`StateRootProvider` and
//! friends). Those follow the header carrying GolemDB's own `state_root`.

use alloy_primitives::{Address, B256, U256};
use arkiv_golemdb_state::accounts::{CELL_BALANCE, CELL_NONCE, record_key};
use arkiv_interfaces::store::{CommitId, ReadTarget, Record, Store, StoreError};

/// An Ethereum account as the store holds it.
///
/// Deliberately not reth's `Account`: this crate should not decide how the
/// absent-versus-zero question maps onto `Option<Account>` while that question
/// is still open (#145). The caller converts, and the conversion is where the
/// rule will live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredAccount {
    /// The account's balance. Absent in the store means zero.
    pub balance: U256,
    /// The account's transaction nonce. Absent in the store means zero.
    pub nonce: u64,
}

/// Why an account read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountReadError {
    /// The store rejected the read.
    Store(StoreError),
    /// A cell was present but the wrong width to decode.
    Malformed(&'static str),
}

impl From<StoreError> for AccountReadError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl core::fmt::Display for AccountReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store read failed: {e:?}"),
            Self::Malformed(cell) => write!(f, "cell {cell} is the wrong width"),
        }
    }
}

impl core::error::Error for AccountReadError {}

/// Reads accounts out of a store at one commit.
///
/// A commit, not a branch: this serves settled reads -- `eth_getBalance`, the
/// txpool's checks -- which ask about committed state. Execution reads through
/// its branch via [`HostStateView`](crate::HostStateView) instead.
#[derive(Debug, Clone, Copy)]
pub struct GolemAccounts<S> {
    store: S,
    at: CommitId,
}

impl<S: Store> GolemAccounts<S> {
    /// Read accounts as of `at`.
    pub const fn new(store: S, at: CommitId) -> Self {
        Self { store, at }
    }

    /// The commit these reads are against.
    pub const fn at(&self) -> CommitId {
        self.at
    }

    fn record(&self, address: Address) -> Result<Option<Record>, AccountReadError> {
        Ok(self
            .store
            .get(
                ReadTarget::Commit(self.at),
                record_key(address.into_array()),
                None,
                None,
            )?
            .into_value())
    }

    /// The account at `address`, or `None` if the store holds no record for it.
    ///
    /// `None` means "no record", which is not yet the same as "no such Ethereum
    /// account" -- a record can exist holding only the minting nonce, written
    /// by a lane that knows nothing about the account's balance. Deciding what
    /// existence means once reth is no longer the authority is tracked in #145;
    /// until then this reports what the store actually holds and leaves the
    /// interpretation to the caller.
    pub fn account(&self, address: Address) -> Result<Option<StoredAccount>, AccountReadError> {
        let Some(record) = self.record(address)? else {
            return Ok(None);
        };
        Ok(Some(StoredAccount {
            balance: balance_of(&record)?,
            nonce: nonce_of(&record)?,
        }))
    }

    /// Whether the store holds any record for `address`.
    pub fn has_record(&self, address: Address) -> Result<bool, AccountReadError> {
        Ok(self.record(address)?.is_some())
    }

    /// Contract code. Always `None`: Arkiv runs no EVM, so no account has any.
    ///
    /// Genesis predeploys no bytecode and `ArkivEvm::transact_raw` intercepts
    /// every user transaction, so there is nothing for this to return. It
    /// exists because a `StateProvider` must answer it.
    pub const fn code(&self, _address: Address) -> Option<&'static [u8]> {
        None
    }

    /// Contract storage. Always `None`, for the same reason as [`code`](Self::code).
    pub const fn storage(&self, _address: Address, _key: B256) -> Option<U256> {
        None
    }
}

/// The balance cell, or zero when absent.
fn balance_of(record: &Record) -> Result<U256, AccountReadError> {
    let Some(cell) = record.cell(CELL_BALANCE) else {
        return Ok(U256::ZERO);
    };
    let bytes: [u8; 32] = cell
        .value
        .as_slice()
        .try_into()
        .map_err(|_| AccountReadError::Malformed(CELL_BALANCE))?;
    Ok(U256::from_be_bytes(bytes))
}

/// The nonce cell, or zero when absent.
fn nonce_of(record: &Record) -> Result<u64, AccountReadError> {
    let Some(cell) = record.cell(CELL_NONCE) else {
        return Ok(0);
    };
    let bytes: [u8; 8] = cell
        .value
        .as_slice()
        .try_into()
        .map_err(|_| AccountReadError::Malformed(CELL_NONCE))?;
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_golemdb_state::GolemStateManager;
    use arkiv_interfaces::primitives::UserBalance;
    use arkiv_interfaces::statemanager::{
        AccountBalancesStore, AccountNoncesStore, BlockRef, EntityCreationNoncesStore, StateManager,
    };
    use arkiv_interfaces::store::StoreExt;
    use arkiv_interfaces::store::reference::MemStore;
    use std::sync::Arc;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const GENESIS: BlockRef = BlockRef {
        height: 0,
        hash: [0; 32],
    };

    /// Write through a view, commit, and read back at that commit.
    #[allow(clippy::arc_with_non_send_sync)] // MemStore is a RefCell; see its docs.
    fn committed(
        write: impl FnOnce(&mut arkiv_golemdb_state::GolemStateView<Arc<MemStore>>),
    ) -> GolemAccounts<Arc<MemStore>> {
        let store = Arc::new(MemStore::new());
        let branch = store.begin(None).expect("begin");
        store
            .commit_tagged(branch, GENESIS.hash)
            .expect("tag genesis");

        let manager = GolemStateManager::new(store.clone());
        let mut view = manager.view(GENESIS).expect("view");
        write(&mut view);
        let commit = manager
            .seal(
                view,
                BlockRef {
                    height: 1,
                    hash: [1; 32],
                },
            )
            .expect("seal");

        GolemAccounts::new(store, commit)
    }

    #[test]
    fn an_account_nobody_wrote_has_no_record() {
        let accounts = committed(|_| {});
        assert_eq!(accounts.account(ALICE).unwrap(), None);
    }

    #[test]
    fn a_balance_and_nonce_survive_the_round_trip() {
        let accounts = committed(|view| {
            view.set_balance(ALICE.into_array(), UserBalance::from_u64(1234))
                .unwrap();
            for _ in 0..7 {
                view.fetch_increment_acc_nonce(ALICE.into_array()).unwrap();
            }
        });
        assert_eq!(
            accounts.account(ALICE).unwrap(),
            Some(StoredAccount {
                balance: U256::from(1234),
                nonce: 7
            })
        );
    }

    /// A balance wider than 64 bits has to survive: truncating here would
    /// silently cap every account.
    #[test]
    fn a_large_balance_survives_the_round_trip() {
        let big = UserBalance::from_be_bytes([0xab; 32]);
        let accounts = committed(|view| {
            view.set_balance(ALICE.into_array(), big).unwrap();
        });
        assert_eq!(
            accounts.account(ALICE).unwrap().unwrap().balance,
            U256::from_be_bytes([0xab; 32])
        );
    }

    /// The case #145 has to resolve: a record written by the minting lane says
    /// nothing about the account's balance, but it is still a record.
    #[test]
    fn a_minting_only_record_reads_as_zero_but_exists() {
        let accounts = committed(|view| {
            view.fetch_increment_entity_creation_nonce(ALICE.into_array())
                .unwrap();
        });
        assert!(accounts.has_record(ALICE).unwrap());
        assert_eq!(
            accounts.account(ALICE).unwrap(),
            Some(StoredAccount {
                balance: U256::ZERO,
                nonce: 0
            }),
            "absent cells read as zero; whether that is an Ethereum account is #145"
        );
    }

    /// Arkiv runs no EVM, so these are constants, and a caller that trusts them
    /// should keep working if that ever stops being true.
    #[test]
    fn no_account_has_code_or_storage() {
        let accounts = committed(|view| {
            view.set_balance(ALICE.into_array(), UserBalance::from_u64(1))
                .unwrap();
        });
        assert_eq!(accounts.code(ALICE), None);
        assert_eq!(accounts.storage(ALICE, B256::ZERO), None);
    }
}
