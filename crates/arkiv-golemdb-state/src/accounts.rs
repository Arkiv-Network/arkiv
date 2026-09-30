//! Balances and the two nonces: plain key/value over the committed store.
//!
//! # The reserved key namespace
//!
//! Addresses are 32 bytes and so are record keys, so account keys and entity keys
//! occupy the same space with nothing structural between them. Account keys are
//! therefore `keccak(DOMAIN_ACCOUNT ‖ address)` — see
//! [`arkiv_interfaces::keys`], which owns the preimage because these bytes reach the
//! state root. [`tests::an_account_key_cannot_be_an_entity_key`] pins the property.
//!
//! The cost of hashing is that a key no longer reveals its address, so the record
//! carries an [`CELL_ADDRESS`] cell and stays self-describing for a state dump.
//!
//! # One record, three cells
//!
//! The three traits are separate, but their storage need not mirror them. All three
//! values hang off the same address, so they live in one record:
//!
//! | cell             | type    | holds                              |
//! |------------------|---------|------------------------------------|
//! | [`CELL_ADDRESS`] | `BYTES` | the account's own address          |
//! | [`CELL_BALANCE`] | `U256`  | the balance                        |
//! | [`CELL_NONCE`]   | `U64`   | the transaction nonce              |
//! | [`CELL_MINTED`]  | `U64`   | the entity-creation nonce          |
//!
//! Reading balance and nonce together — which every transaction does, to check the
//! sender can pay — is then one `get` rather than two. `patch` updates a single cell,
//! so writing one value does not rewrite the others.
//!
//! All three are **fields, not attributes**: nothing queries accounts through the
//! Arkiv query language, so indexing them would be paid for and never used.
//!
//! An absent cell reads as zero, which is the Ethereum convention and what a
//! never-before-seen account must look like. Every cell is written on creation so
//! that absent-versus-zero never has to be distinguished on the read path.

use alloc::vec;
use alloc::vec::Vec;

use alloy_primitives::keccak256;
use arkiv_interfaces::keys::account_key_preimage;
use arkiv_interfaces::primitives::{EntityCreationNonce, UserAddress, UserBalance, UserNonce};
use arkiv_interfaces::statemanager::{Commitment, ReadMode};
use arkiv_interfaces::store::{
    BranchId, Cell, CellChange, CellName, CommitId, ReadTarget, Record, RecordKey, Store,
    StoreError, TypeId,
};

/// The account's own address, so a hashed key stays reversible.
pub const CELL_ADDRESS: &str = "adr";
/// The account's balance.
pub const CELL_BALANCE: &str = "bal";
/// The account's transaction nonce.
pub const CELL_NONCE: &str = "non";
/// The account's entity-creation nonce.
pub const CELL_MINTED: &str = "mnt";

/// Why an account operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    /// The store rejected the read or write.
    Store(StoreError),
    /// A cell was present but the wrong width to decode.
    Malformed(&'static str),
    /// A balance change overflowed or underflowed.
    ///
    /// Not clamped: a balance that saturates silently is money created or destroyed.
    BalanceOutOfRange,
}

impl From<StoreError> for AccountError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Widen a host address into the 32-byte form the store keys on.
///
/// `UserAddress` is still 20 bytes in `arkiv-interfaces`; the store keys on 32.
/// One place does the widening so that when the primitive widens, only this
/// function changes — and it is left-padding, matching ABI convention, so the two
/// forms name the same account.
pub fn wide(account: UserAddress) -> [u8; 32] {
    let mut wide = [0u8; 32];
    wide[12..].copy_from_slice(&account);
    wide
}

/// The record key for an account: `keccak(DOMAIN_ACCOUNT ‖ address)`.
pub fn record_key(account: UserAddress) -> RecordKey {
    RecordKey::from_entity(keccak256(account_key_preimage(&wide(account))).into())
}

/// The cells of a fresh account record, all three written explicitly.
fn fresh_cells(
    account: UserAddress,
    balance: UserBalance,
    nonce: UserNonce,
    minted: EntityCreationNonce,
) -> Vec<(CellName, Cell)> {
    vec![
        (
            CellName::from(CELL_ADDRESS),
            Cell::field(TypeId::BYTES, wide(account).to_vec()),
        ),
        (
            CellName::from(CELL_BALANCE),
            Cell::field(TypeId::U256, balance.to_be_bytes().to_vec()),
        ),
        (
            CellName::from(CELL_NONCE),
            Cell::field(TypeId::U64, nonce.get().to_be_bytes().to_vec()),
        ),
        (
            CellName::from(CELL_MINTED),
            Cell::field(TypeId::U64, minted.get().to_be_bytes().to_vec()),
        ),
    ]
}

fn u64_cell(record: Option<&Record>, name: &'static str) -> Result<u64, AccountError> {
    let Some(cell) = record.and_then(|record| record.cell(name)) else {
        return Ok(0);
    };
    cell.value
        .as_slice()
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| AccountError::Malformed(name))
}

/// Balances and nonces over a GolemDB [`Store`].
#[derive(Debug)]
pub struct StoreAccounts<S: Store> {
    store: S,
    branch: BranchId,
    origin: CommitId,
}

impl<S: Store> StoreAccounts<S> {
    /// Stage writes on `branch`, whose base state is the commit `origin`.
    pub const fn new(store: S, branch: BranchId, origin: CommitId) -> Self {
        Self {
            store,
            branch,
            origin,
        }
    }

    /// Give the store back.
    pub fn into_store(self) -> S {
        self.store
    }

    const fn target(&self, read: ReadMode) -> ReadTarget {
        match read {
            ReadMode::ViewOnBase => ReadTarget::Commit(self.origin),
            ReadMode::ViewWithOverlay => ReadTarget::Branch(self.branch),
        }
    }

    fn read(&self, account: UserAddress, read: ReadMode) -> Result<Option<Record>, AccountError> {
        Ok(self
            .store
            .get(self.target(read), record_key(account), None, None)?
            .into_value())
    }

    /// The account's balance. Absent is zero.
    pub fn balance(
        &self,
        account: UserAddress,
        read: ReadMode,
    ) -> Result<UserBalance, AccountError> {
        let record = self.read(account, read)?;
        let Some(cell) = record.as_ref().and_then(|record| record.cell(CELL_BALANCE)) else {
            return Ok(UserBalance::from_u64(0));
        };
        cell.value
            .as_slice()
            .try_into()
            .map(UserBalance::from_be_bytes)
            .map_err(|_| AccountError::Malformed(CELL_BALANCE))
    }

    /// The account's transaction nonce. Absent is zero.
    pub fn nonce(&self, account: UserAddress, read: ReadMode) -> Result<UserNonce, AccountError> {
        Ok(UserNonce::new(u64_cell(
            self.read(account, read)?.as_ref(),
            CELL_NONCE,
        )?))
    }

    /// The account's entity-creation nonce. Absent is zero.
    pub fn minted(
        &self,
        account: UserAddress,
        read: ReadMode,
    ) -> Result<EntityCreationNonce, AccountError> {
        Ok(EntityCreationNonce::new(u64_cell(
            self.read(account, read)?.as_ref(),
            CELL_MINTED,
        )?))
    }

    /// Write one cell, creating the record if the account is new.
    fn write_cell(
        &mut self,
        account: UserAddress,
        name: &str,
        cell: Cell,
    ) -> Result<(), AccountError> {
        let key = record_key(account);
        if self.read(account, ReadMode::ViewWithOverlay)?.is_none() {
            // New account: write all three cells so the read path never has to tell
            // an absent cell from a zero one.
            let mut cells = fresh_cells(
                account,
                UserBalance::from_u64(0),
                UserNonce::new(0),
                EntityCreationNonce::new(0),
            );
            for (existing, value) in &mut cells {
                if existing == name {
                    *value = cell.clone();
                }
            }
            self.store.create(self.branch, key, cells, None)?;
            return Ok(());
        }
        self.store.patch(
            self.branch,
            key,
            None,
            vec![(CellName::from(name), CellChange::Set(cell))],
            None,
        )?;
        Ok(())
    }

    /// Set the balance outright.
    pub fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), AccountError> {
        let cell = Cell::field(TypeId::U256, balance.to_be_bytes().to_vec());
        self.write_cell(account, CELL_BALANCE, cell)
    }

    /// Add to the balance, returning the value **before** the addition.
    pub fn fetch_add_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, AccountError> {
        let before = self.balance(account, ReadMode::ViewWithOverlay)?;
        // `checked_`, never `saturating_`: a balance that clamps at the maximum has
        // created money, and doing so silently is worse than refusing the write.
        let after = before
            .checked_add(amount)
            .ok_or(AccountError::BalanceOutOfRange)?;
        self.set_balance(account, after)?;
        Ok(before)
    }

    /// Subtract from the balance, returning the value **before** the subtraction.
    pub fn fetch_sub_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, AccountError> {
        let before = self.balance(account, ReadMode::ViewWithOverlay)?;
        let after = before
            .checked_sub(amount)
            .ok_or(AccountError::BalanceOutOfRange)?;
        self.set_balance(account, after)?;
        Ok(before)
    }

    /// Set the balance to `new` only if it is currently `current`.
    pub fn compare_set_balance(
        &mut self,
        account: UserAddress,
        current: UserBalance,
        new: UserBalance,
    ) -> Result<bool, AccountError> {
        if self.balance(account, ReadMode::ViewWithOverlay)? != current {
            return Ok(false);
        }
        self.set_balance(account, new)?;
        Ok(true)
    }

    /// Advance the transaction nonce, returning the value **before**.
    pub fn fetch_increment_nonce(
        &mut self,
        account: UserAddress,
    ) -> Result<UserNonce, AccountError> {
        let before = self.nonce(account, ReadMode::ViewWithOverlay)?;
        let cell = Cell::field(TypeId::U64, before.next().get().to_be_bytes().to_vec());
        self.write_cell(account, CELL_NONCE, cell)?;
        Ok(before)
    }

    /// Advance the entity-creation nonce, returning the value **before**.
    pub fn fetch_increment_minted(
        &mut self,
        account: UserAddress,
    ) -> Result<EntityCreationNonce, AccountError> {
        let before = self.minted(account, ReadMode::ViewWithOverlay)?;
        let cell = Cell::field(
            TypeId::U64,
            before.advanced_by(1).get().to_be_bytes().to_vec(),
        );
        self.write_cell(account, CELL_MINTED, cell)?;
        Ok(before)
    }

    /// The branch's content digest.
    pub fn commit(&self) -> Result<Commitment, AccountError> {
        Ok(self.store.branch_digest(self.branch)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity_records;
    use arkiv_interfaces::store::reference::MemStore;

    const ALICE: UserAddress = [0xaa; 20];
    const BOB: UserAddress = [0xbb; 20];

    fn view() -> StoreAccounts<MemStore> {
        let mut store = MemStore::default();
        let branch = store.begin(None).expect("begin");
        let origin = store.head();
        StoreAccounts::new(store, branch, origin)
    }

    #[test]
    fn an_unseen_account_reads_as_zero() {
        let view = view();
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(0)
        );
        assert_eq!(
            view.nonce(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserNonce::new(0)
        );
        assert_eq!(
            view.minted(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            EntityCreationNonce::new(0)
        );
    }

    #[test]
    fn an_account_key_cannot_be_an_entity_key() {
        // Addresses and record keys are both 32 bytes, so nothing structural keeps
        // the two spaces apart -- only the domain tag does. The sharpest case is an
        // entity whose address *is* an account's: the keys must still differ.
        let shared = wide(ALICE);
        assert_ne!(record_key(ALICE), entity_records::record_key(shared));

        // And a bare address is never an account key, which is what a caller
        // reaching for the old padding convention would produce.
        assert_ne!(record_key(ALICE), RecordKey::from_entity(shared));
    }

    #[test]
    fn a_hashed_key_stays_reversible_through_the_record() {
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(1)).unwrap();
        let record = view
            .read(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .expect("the account exists");
        assert_eq!(
            record.cell(CELL_ADDRESS).unwrap().value,
            wide(ALICE).to_vec()
        );
    }

    #[test]
    fn the_three_values_are_independent() {
        // They share a record, so writing one must not disturb the others.
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(500)).unwrap();
        view.fetch_increment_nonce(ALICE).unwrap();
        view.fetch_increment_minted(ALICE).unwrap();
        view.fetch_increment_minted(ALICE).unwrap();

        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(500)
        );
        assert_eq!(
            view.nonce(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserNonce::new(1)
        );
        assert_eq!(
            view.minted(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            EntityCreationNonce::new(2)
        );
    }

    #[test]
    fn accounts_do_not_leak_into_each_other() {
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(10)).unwrap();
        assert_eq!(
            view.balance(BOB, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(0)
        );
    }

    #[test]
    fn fetch_ops_return_the_value_before_the_change() {
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(100)).unwrap();

        let before = view
            .fetch_add_balance(ALICE, UserBalance::from_u64(5))
            .unwrap();
        assert_eq!(
            before,
            UserBalance::from_u64(100),
            "the trait says *before*"
        );
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(105)
        );

        let before = view
            .fetch_sub_balance(ALICE, UserBalance::from_u64(5))
            .unwrap();
        assert_eq!(before, UserBalance::from_u64(105));
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(100)
        );

        let before = view.fetch_increment_nonce(ALICE).unwrap();
        assert_eq!(before, UserNonce::new(0));
        assert_eq!(
            view.nonce(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserNonce::new(1)
        );
    }

    #[test]
    fn an_overdraft_is_refused_not_clamped() {
        // Saturating here would destroy money and leave no trace.
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(10)).unwrap();
        assert_eq!(
            view.fetch_sub_balance(ALICE, UserBalance::from_u64(11)),
            Err(AccountError::BalanceOutOfRange)
        );
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(10),
            "and the balance is untouched"
        );
    }

    #[test]
    fn an_overflow_is_refused_not_clamped() {
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_be_bytes([0xff; 32]))
            .unwrap();
        assert_eq!(
            view.fetch_add_balance(ALICE, UserBalance::from_u64(1)),
            Err(AccountError::BalanceOutOfRange)
        );
    }

    #[test]
    fn compare_set_only_fires_on_a_match() {
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(10)).unwrap();
        assert!(
            !view
                .compare_set_balance(ALICE, UserBalance::from_u64(99), UserBalance::from_u64(1))
                .unwrap()
        );
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(10)
        );
        assert!(
            view.compare_set_balance(ALICE, UserBalance::from_u64(10), UserBalance::from_u64(1))
                .unwrap()
        );
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(1)
        );
    }

    #[test]
    fn base_reads_do_not_see_staged_writes() {
        let mut view = view();
        view.set_balance(ALICE, UserBalance::from_u64(10)).unwrap();
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewOnBase).unwrap(),
            UserBalance::from_u64(0)
        );
    }

    #[test]
    fn a_large_balance_survives_the_round_trip() {
        // 32 bytes, not a u64: truncating here would silently cap every balance.
        let mut view = view();
        let big = UserBalance::from_be_bytes([0xab; 32]);
        view.set_balance(ALICE, big).unwrap();
        assert_eq!(view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(), big);
    }
}
