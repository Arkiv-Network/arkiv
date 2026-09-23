//! Opt-in prototype adapter. This is not yet selected by `write_manager` or the
//! running node. A successful operation returns ordinary revm state containing
//! native-account changes and a single Arkiv commitment slot. Custom records are
//! persisted first; an abandoned proposal can only leave unreachable records.

use crate::WriteOverlay;
use alloy_primitives::{Address, B256, U256, keccak256};
use arkiv_authenticated_store::{RecordStore, State};
use reth_ethereum::evm::{primitives::Database, revm::state::EvmState};

/// Dedicated system account in the new-chain layout. No entity/index account
/// derivation is used by the authenticated store.
pub const ROOT_ACCOUNT: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x46,
]);

pub fn root_slot() -> U256 {
    U256::from_be_bytes(keccak256(b"arkiv.authenticated.root.v1").0)
}

pub struct AuthenticatedOverlay<'a, DB: Database, S: RecordStore> {
    ethereum: WriteOverlay<'a, DB>,
    arkiv: State<S>,
    parent_root: B256,
}

impl<'a, DB: Database, S: RecordStore> AuthenticatedOverlay<'a, DB, S> {
    /// The root always comes from the supplied Ethereum state view. Passing a
    /// historical/branch view opens that exact Arkiv snapshot, not a global head.
    pub fn open(db: &'a mut DB, store: S) -> eyre::Result<Self> {
        let mut ethereum = WriteOverlay::new(db);
        let parent_root = B256::from(
            ethereum
                .read_slot(ROOT_ACCOUNT, root_slot())?
                .to_be_bytes::<32>(),
        );
        let arkiv = State::open(store, parent_root)?;
        Ok(Self {
            ethereum,
            arkiv,
            parent_root,
        })
    }

    /// Execute an isolated batch and return the native diff only after the custom
    /// records are durable. On any error both logical overlays are discarded.
    /// Applying/validating the returned diff is the host's responsibility, as is
    /// charging fees for a reverted transaction in a separate successful batch.
    pub fn execute<T>(
        mut self,
        operation: impl FnOnce(&mut State<S>, &mut WriteOverlay<'a, DB>) -> eyre::Result<T>,
    ) -> eyre::Result<(T, EvmState)> {
        let result = operation(&mut self.arkiv, &mut self.ethereum)?;
        let root = self.arkiv.persist()?;
        if root != self.parent_root {
            self.ethereum.persist_account(ROOT_ACCOUNT)?;
            self.ethereum
                .write_slot(ROOT_ACCOUNT, root_slot(), U256::from_be_bytes(root.0))?;
        }
        Ok((result, self.ethereum.into_state()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_authenticated_store::{EMPTY_ROOT, MemoryStore};
    use arkiv_interfaces::entity::{Attribute, AttributeValue, Entity};
    use arkiv_reth_mpt_committed_store::{BalanceAccess, NonceAccess};
    use reth_ethereum::evm::revm::{
        database_interface::{DatabaseCommit, EmptyDB},
        db::CacheDB,
        state::AccountInfo,
    };

    #[test]
    fn only_native_accounts_and_root_enter_ethereum_state() -> eyre::Result<()> {
        let alice = Address::repeat_byte(1);
        let bob = Address::repeat_byte(2);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            alice,
            AccountInfo {
                balance: U256::from(100),
                nonce: 7,
                ..Default::default()
            },
        );
        let parent_db = db.clone();
        let store = MemoryStore::default();
        let entity = Entity {
            key: [9; 32],
            creator: alice.into_array(),
            owner: alice.into_array(),
            attributes: vec![Attribute::new(b"age".to_vec(), AttributeValue::Int(35))],
            ..Default::default()
        };
        let (root, diff) =
            AuthenticatedOverlay::open(&mut db, store.clone())?.execute(|arkiv, native| {
                arkiv.put_entity(&entity)?;
                arkiv.increment_creation_nonce(alice.into_array())?;
                native.set_balance(alice, U256::from(90))?;
                native.set_balance(bob, U256::from(10))?;
                native.set_nonce(alice, 8)?;
                Ok(arkiv.root())
            })?;
        assert_ne!(root, EMPTY_ROOT);
        assert_eq!(diff.len(), 3);
        assert!(diff.keys().all(|a| [alice, bob, ROOT_ACCOUNT].contains(a)));
        assert_eq!(diff[&alice].info.balance, U256::from(90));
        assert_eq!(diff[&alice].info.nonce, 8);
        let system = &diff[&ROOT_ACCOUNT];
        assert_eq!(system.storage.len(), 1);
        assert_eq!(
            system.storage[&root_slot()].present_value,
            U256::from_be_bytes(root.0)
        );
        assert!(system.info.code.as_ref().is_none_or(|c| c.is_empty()));
        db.commit(diff);
        let (_, no_changes) =
            AuthenticatedOverlay::open(&mut db, store.clone())?.execute(|arkiv, native| {
                assert_eq!(arkiv.entity(entity.key)?, Some(entity.clone()));
                assert_eq!(arkiv.creation_nonce(alice.into_array())?, 1);
                assert_eq!(native.get_nonce(alice)?, 8);
                assert_eq!(native.get_balance(bob)?, U256::from(10));
                Ok(())
            })?;
        assert!(no_changes.is_empty());
        // Reorg selection comes solely from the parent Ethereum view.
        let mut parent_db = parent_db;
        AuthenticatedOverlay::open(&mut parent_db, store)?.execute(|arkiv, native| {
            assert_eq!(arkiv.root(), EMPTY_ROOT);
            assert!(arkiv.entity(entity.key)?.is_none());
            assert_eq!(native.get_nonce(alice)?, 7);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn failed_operation_discards_native_and_custom_changes() -> eyre::Result<()> {
        let mut db = CacheDB::new(EmptyDB::default());
        let store = MemoryStore::default();
        let alice = Address::repeat_byte(1);
        let result =
            AuthenticatedOverlay::open(&mut db, store.clone())?.execute::<()>(|arkiv, native| {
                arkiv.increment_creation_nonce(alice.into_array())?;
                native.set_balance(alice, U256::from(999))?;
                eyre::bail!("transaction reverted")
            });
        assert!(result.is_err());
        AuthenticatedOverlay::open(&mut db, store)?.execute(|arkiv, native| {
            assert_eq!(arkiv.root(), EMPTY_ROOT);
            assert_eq!(arkiv.creation_nonce(alice.into_array())?, 0);
            assert_eq!(native.get_balance(alice)?, U256::ZERO);
            Ok(())
        })?;
        Ok(())
    }
}
