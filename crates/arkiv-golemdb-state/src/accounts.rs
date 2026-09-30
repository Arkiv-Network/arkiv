//! Balances and the two nonces: plain key/value over the committed store.
//!
//! # The reserved key namespace
//!
//! Account state needs a key space that can never collide with an entity's. The
//! seam already has one: [`RecordKey::from_address`] left-pads a 20-byte address
//! with **twelve zero bytes**, and [`RecordKey::as_address`] reverses it. Those
//! twelve zeros are the reservation — an [`EntityAddress`] is a derived 32-byte
//! value, so one landing in that range is a hash collision rather than a layout
//! mistake, and [`tests::an_account_key_cannot_be_an_entity_key`] pins the property.
//!
//! [`EntityAddress`]: arkiv_interfaces::primitives::EntityAddress
//!
//! # One record, three cells
//!
//! The three traits are separate, but their storage need not mirror them. All three
//! values hang off the same address, so they live in one record:
//!
//! | cell            | type   | holds                               |
//! |-----------------|--------|-------------------------------------|
//! | [`CELL_BALANCE`]| `U256` | the balance                         |
//! | [`CELL_NONCE`]  | `U64`  | the transaction nonce               |
//! | [`CELL_MINTED`] | `U64`  | the entity-creation nonce           |
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

use arkiv_interfaces::primitives::{EntityCreationNonce, UserAddress, UserBalance, UserNonce};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, AccountNoncesStore, Commitment, EntityCreationNoncesStore, ReadMode,
};
use arkiv_interfaces::store::{
    Cell, CellChange, CellName, Record, RecordKey, Store, StoreError, TypeId,
};

use crate::view::{GolemStateView, ViewError};

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

/// The record key for an account. See the module docs on the reserved namespace.
pub const fn record_key(account: UserAddress) -> RecordKey {
    RecordKey::from_address(account)
}

/// The cells of a fresh account record, all three written explicitly.
fn fresh_cells(
    balance: UserBalance,
    nonce: UserNonce,
    minted: EntityCreationNonce,
) -> Vec<(CellName, Cell)> {
    vec![
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

impl<S: Store> GolemStateView<S> {
    fn read(&self, account: UserAddress, read: ReadMode) -> Result<Option<Record>, AccountError> {
        Ok(self
            .store
            .get(self.target(read), record_key(account), None, None)?
            .into_value())
    }

    /// The account's balance. Absent is zero.
    fn balance(&self, account: UserAddress, read: ReadMode) -> Result<UserBalance, AccountError> {
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
    fn nonce(&self, account: UserAddress, read: ReadMode) -> Result<UserNonce, AccountError> {
        Ok(UserNonce::new(u64_cell(
            self.read(account, read)?.as_ref(),
            CELL_NONCE,
        )?))
    }

    /// The account's entity-creation nonce. Absent is zero.
    fn minted(
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
    fn write_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), AccountError> {
        let cell = Cell::field(TypeId::U256, balance.to_be_bytes().to_vec());
        self.write_cell(account, CELL_BALANCE, cell)
    }

    /// Add to the balance, returning the value **before** the addition.
    fn add_balance(
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
        self.write_balance(account, after)?;
        Ok(before)
    }

    /// Subtract from the balance, returning the value **before** the subtraction.
    fn sub_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, AccountError> {
        let before = self.balance(account, ReadMode::ViewWithOverlay)?;
        let after = before
            .checked_sub(amount)
            .ok_or(AccountError::BalanceOutOfRange)?;
        self.write_balance(account, after)?;
        Ok(before)
    }

    /// Set the balance to `new` only if it is currently `current`.
    fn cas_balance(
        &mut self,
        account: UserAddress,
        current: UserBalance,
        new: UserBalance,
    ) -> Result<bool, AccountError> {
        if self.balance(account, ReadMode::ViewWithOverlay)? != current {
            return Ok(false);
        }
        self.write_balance(account, new)?;
        Ok(true)
    }

    /// Advance the transaction nonce, returning the value **before**.
    fn fetch_increment_nonce(&mut self, account: UserAddress) -> Result<UserNonce, AccountError> {
        let before = self.nonce(account, ReadMode::ViewWithOverlay)?;
        let cell = Cell::field(TypeId::U64, before.next().get().to_be_bytes().to_vec());
        self.write_cell(account, CELL_NONCE, cell)?;
        Ok(before)
    }

    /// Advance the entity-creation nonce, returning the value **before**.
    fn fetch_increment_minted(
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
}

impl<S: Store> AccountBalancesStore for GolemStateView<S> {
    type Error = ViewError;

    fn get_balance(&self, account: UserAddress, read: ReadMode) -> Result<UserBalance, ViewError> {
        self.balance(account, read).map_err(Into::into)
    }

    fn fetch_add_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, ViewError> {
        self.add_balance(account, amount).map_err(Into::into)
    }

    fn fetch_sub_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, ViewError> {
        self.sub_balance(account, amount).map_err(Into::into)
    }

    fn compare_set_balance(
        &mut self,
        account: UserAddress,
        current: UserBalance,
        new: UserBalance,
    ) -> Result<bool, ViewError> {
        self.cas_balance(account, current, new).map_err(Into::into)
    }

    fn set_balance(&mut self, account: UserAddress, balance: UserBalance) -> Result<(), ViewError> {
        self.write_balance(account, balance).map_err(Into::into)
    }

    fn commit_store(&mut self) -> Result<Commitment, ViewError> {
        self.shared_commitment()
    }
}

impl<S: Store> AccountNoncesStore for GolemStateView<S> {
    type Error = ViewError;

    fn get_acc_nonce(&self, account: UserAddress, read: ReadMode) -> Result<UserNonce, ViewError> {
        self.nonce(account, read).map_err(Into::into)
    }

    fn fetch_increment_acc_nonce(&mut self, account: UserAddress) -> Result<UserNonce, ViewError> {
        self.fetch_increment_nonce(account).map_err(Into::into)
    }

    fn commit_store(&mut self) -> Result<Commitment, ViewError> {
        self.shared_commitment()
    }
}

impl<S: Store> EntityCreationNoncesStore for GolemStateView<S> {
    type Error = ViewError;

    fn get_entity_creation_nonce(
        &self,
        owner: UserAddress,
        read: ReadMode,
    ) -> Result<EntityCreationNonce, ViewError> {
        self.minted(owner, read).map_err(Into::into)
    }

    fn fetch_increment_entity_creation_nonce(
        &mut self,
        owner: UserAddress,
    ) -> Result<EntityCreationNonce, ViewError> {
        self.fetch_increment_minted(owner).map_err(Into::into)
    }

    fn commit_store(&mut self) -> Result<Commitment, ViewError> {
        self.shared_commitment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity_records;

    const ALICE: UserAddress = [0xaa; 20];
    const BOB: UserAddress = [0xbb; 20];

    use crate::view::tests::view;

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
        // The reservation the whole layout rests on: account keys have twelve
        // leading zeros and are reversible; an entity key is a derived 32 bytes.
        let account = record_key(ALICE);
        assert_eq!(&account.0[..12], &[0u8; 12]);
        assert_eq!(account.as_address(), Some(ALICE));

        let entity = entity_records::record_key([0x37; 32]);
        assert_ne!(account, entity);
        assert_eq!(
            entity.as_address(),
            None,
            "an entity key is not in the reserved range"
        );
    }

    #[test]
    fn the_three_values_are_independent() {
        // They share a record, so writing one must not disturb the others.
        let mut view = view();
        view.write_balance(ALICE, UserBalance::from_u64(500))
            .unwrap();
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
        view.write_balance(ALICE, UserBalance::from_u64(10))
            .unwrap();
        assert_eq!(
            view.balance(BOB, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(0)
        );
    }

    #[test]
    fn fetch_ops_return_the_value_before_the_change() {
        let mut view = view();
        view.write_balance(ALICE, UserBalance::from_u64(100))
            .unwrap();

        let before = view.add_balance(ALICE, UserBalance::from_u64(5)).unwrap();
        assert_eq!(
            before,
            UserBalance::from_u64(100),
            "the trait says *before*"
        );
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(105)
        );

        let before = view.sub_balance(ALICE, UserBalance::from_u64(5)).unwrap();
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
        view.write_balance(ALICE, UserBalance::from_u64(10))
            .unwrap();
        assert_eq!(
            view.sub_balance(ALICE, UserBalance::from_u64(11)),
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
        view.write_balance(ALICE, UserBalance::from_be_bytes([0xff; 32]))
            .unwrap();
        assert_eq!(
            view.add_balance(ALICE, UserBalance::from_u64(1)),
            Err(AccountError::BalanceOutOfRange)
        );
    }

    #[test]
    fn compare_set_only_fires_on_a_match() {
        let mut view = view();
        view.write_balance(ALICE, UserBalance::from_u64(10))
            .unwrap();
        assert!(
            !view
                .cas_balance(ALICE, UserBalance::from_u64(99), UserBalance::from_u64(1))
                .unwrap()
        );
        assert_eq!(
            view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(10)
        );
        assert!(
            view.cas_balance(ALICE, UserBalance::from_u64(10), UserBalance::from_u64(1))
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
        view.write_balance(ALICE, UserBalance::from_u64(10))
            .unwrap();
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
        view.write_balance(ALICE, big).unwrap();
        assert_eq!(view.balance(ALICE, ReadMode::ViewWithOverlay).unwrap(), big);
    }
}
