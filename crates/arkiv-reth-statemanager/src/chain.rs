//! Live-chain state view: Arkiv records in an authenticated store, native
//! balances/nonces in the ordinary Ethereum execution overlay.
use crate::{
    MptStateView, WriteOverlay,
    authenticated::{ROOT_ACCOUNT, root_slot},
};
use alloy_primitives::B256;
use arkiv_authenticated_store::{State, Store};
use arkiv_interfaces::{
    entity::{AttributeValue, Entity},
    gas::PlaceholderCost,
    primitives::{EntityAddress, EntityCreationNonce, UserAddress, UserBalance, UserNonce},
    statemanager::*,
};
use reth_ethereum::evm::primitives::Database;
use std::{collections::BTreeMap, ops::Bound};

pub struct ChainView<'a, DB: Database> {
    native: MptStateView<WriteOverlay<'a, DB>>,
    base: State<Store>,
    staged: State<Store>,
    updates: BTreeMap<EntityAddress, EntityUpdates>,
}

fn native_error(error: impl std::fmt::Debug) -> eyre::Report {
    eyre::eyre!("native state: {error:?}")
}

pub fn chain_manager<DB: Database>(
    db: &mut DB,
    block: BlockRef,
    store: Store,
) -> eyre::Result<ChainView<'_, DB>> {
    let mut overlay = WriteOverlay::new(db);
    let root = B256::from(
        overlay
            .read_slot(ROOT_ACCOUNT, root_slot())?
            .to_be_bytes::<32>(),
    );
    let base = State::open(store, root)?;
    Ok(ChainView {
        native: MptStateView::new(overlay, block),
        staged: base.clone(),
        base,
        updates: BTreeMap::new(),
    })
}

impl<'a, DB: Database> ChainView<'a, DB> {
    pub fn cost_model(&self) -> &PlaceholderCost {
        self.native.cost_model()
    }
    pub fn into_base(self) -> WriteOverlay<'a, DB> {
        self.native.into_base()
    }
    fn state(&self, read: ReadMode) -> &State<Store> {
        if read == ReadMode::ViewOnBase {
            &self.base
        } else {
            &self.staged
        }
    }
    fn flush(&mut self) -> eyre::Result<Commitment> {
        let root = self.staged.persist()?;
        if root != self.base.root() {
            self.native.backend_mut().persist_account(ROOT_ACCOUNT)?;
            self.native
                .backend_mut()
                .write_slot(ROOT_ACCOUNT, root_slot(), root.into())?;
        }
        self.base = self.staged.clone();
        self.updates.clear();
        Ok(root.0)
    }
}

impl<DB: Database> EntityStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn get_entity(&self, address: EntityAddress, read: ReadMode) -> eyre::Result<Option<Entity>> {
        self.state(read).entity(address)
    }
    fn update_entity(&mut self, updates: EntityUpdates) -> eyre::Result<()> {
        if updates.delete {
            self.staged.remove_entity(updates.entity)?;
            self.updates.insert(updates.entity, updates);
        } else {
            let mut entity = self.staged.entity(updates.entity)?.unwrap_or_default();
            updates.apply_to(&mut entity);
            self.staged.put_entity(&entity)?;
            self.updates
                .insert(updates.entity, EntityUpdates::create(entity));
        }
        Ok(())
    }
    fn get_uncommitted_deltas(&self) -> eyre::Result<Vec<EntityUpdates>> {
        Ok(self.updates.values().cloned().collect())
    }
    fn commit_store(&mut self) -> eyre::Result<Commitment> {
        self.flush()
    }
}

impl<DB: Database> EqualityIndexStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn get_equal_entities(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
        read: ReadMode,
    ) -> eyre::Result<Vec<EntityAddress>> {
        let state = self.state(read);
        state.keys(&state.equal(attribute, value)?)
    }
    fn get_prefixed_entities(
        &self,
        attribute: &[u8],
        prefix: &str,
        read: ReadMode,
    ) -> eyre::Result<Vec<EntityAddress>> {
        let state = self.state(read);
        state.keys(&state.prefix(attribute, prefix)?.0)
    }
    // Entity writes maintain the index atomically, so the legacy explicit delta
    // notification must not apply a second copy of the updates.
    fn apply_deltas(&mut self, _: &[EntityUpdates]) -> eyre::Result<()> {
        Ok(())
    }
    fn commit_store(&mut self) -> eyre::Result<()> {
        Ok(())
    }
}

impl<DB: Database> RangeIndexStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn get_within_range(
        &self,
        attribute: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
        read: ReadMode,
    ) -> eyre::Result<Vec<EntityAddress>> {
        let value = |b| match b {
            Bound::Included(v) | Bound::Excluded(v) => Some(v),
            Bound::Unbounded => None,
        };
        let ty = match (value(low), value(high)) {
            (Some(a), Some(b)) if a.attr_type() != b.attr_type() => return Ok(vec![]),
            (Some(v), _) | (_, Some(v)) => v.attr_type(),
            _ => eyre::bail!("range needs a typed bound"),
        };
        let state = self.state(read);
        state.keys(&state.range(attribute, ty, low, high)?.0)
    }
    fn apply_deltas(&mut self, _: &[EntityUpdates]) -> eyre::Result<()> {
        Ok(())
    }
    fn commit_store(&mut self) -> eyre::Result<()> {
        Ok(())
    }
}

impl<DB: Database> EntityCreationNoncesStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn get_entity_creation_nonce(
        &self,
        owner: UserAddress,
        read: ReadMode,
    ) -> eyre::Result<EntityCreationNonce> {
        Ok(EntityCreationNonce::new(
            self.state(read).creation_nonce(owner)?,
        ))
    }
    fn fetch_increment_entity_creation_nonce(
        &mut self,
        owner: UserAddress,
    ) -> eyre::Result<EntityCreationNonce> {
        Ok(EntityCreationNonce::new(
            self.staged.increment_creation_nonce(owner)?,
        ))
    }
    fn commit_store(&mut self) -> eyre::Result<Commitment> {
        self.flush()
    }
}

impl<DB: Database> AccountBalancesStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn get_balance(&self, a: UserAddress, r: ReadMode) -> eyre::Result<UserBalance> {
        self.native.get_balance(a, r).map_err(native_error)
    }
    fn fetch_add_balance(&mut self, a: UserAddress, n: UserBalance) -> eyre::Result<UserBalance> {
        self.native.fetch_add_balance(a, n).map_err(native_error)
    }
    fn fetch_sub_balance(&mut self, a: UserAddress, n: UserBalance) -> eyre::Result<UserBalance> {
        self.native.fetch_sub_balance(a, n).map_err(native_error)
    }
    fn compare_set_balance(
        &mut self,
        a: UserAddress,
        old: UserBalance,
        n: UserBalance,
    ) -> eyre::Result<bool> {
        self.native
            .compare_set_balance(a, old, n)
            .map_err(native_error)
    }
    fn set_balance(&mut self, a: UserAddress, n: UserBalance) -> eyre::Result<()> {
        self.native.set_balance(a, n).map_err(native_error)
    }
    fn commit_store(&mut self) -> eyre::Result<Commitment> {
        AccountBalancesStore::commit_store(&mut self.native).map_err(native_error)
    }
}

impl<DB: Database> AccountNoncesStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn get_acc_nonce(&self, a: UserAddress, r: ReadMode) -> eyre::Result<UserNonce> {
        self.native.get_acc_nonce(a, r).map_err(native_error)
    }
    fn fetch_increment_acc_nonce(&mut self, a: UserAddress) -> eyre::Result<UserNonce> {
        self.native
            .fetch_increment_acc_nonce(a)
            .map_err(native_error)
    }
    fn commit_store(&mut self) -> eyre::Result<Commitment> {
        AccountNoncesStore::commit_store(&mut self.native).map_err(native_error)
    }
}

impl<DB: Database> PruningStore for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn add_to_pruning_set(&mut self, e: EntityAddress, meta: PruningMeta) -> eyre::Result<()> {
        self.native
            .add_to_pruning_set(e, meta)
            .map_err(native_error)
    }
    fn peek_top(
        &self,
        top: u16,
        read: ReadMode,
    ) -> eyre::Result<Vec<(EntityAddress, PruningMeta)>> {
        self.native.peek_top(top, read).map_err(native_error)
    }
    fn take_top(&mut self, top: u16) -> eyre::Result<Vec<(EntityAddress, PruningMeta)>> {
        self.native.take_top(top).map_err(native_error)
    }
    fn commit_store(&mut self) -> eyre::Result<()> {
        PruningStore::commit_store(&mut self.native).map_err(native_error)
    }
}

impl<DB: Database> StateView for ChainView<'_, DB> {
    type Error = eyre::Report;
    fn base(&self) -> BlockRef {
        self.native.base()
    }
    fn session(&self) -> SessionId {
        self.native.session()
    }
    fn has_store(&self, _: StoreKind) -> bool {
        true
    }
    fn graduate(self, block: BlockRef) -> eyre::Result<StateCommit> {
        eyre::ensure!(
            self.staged.root() == self.base.root(),
            "uncommitted Arkiv changes"
        );
        let root = self.base.root().0;
        let mut commit = self.native.graduate(block).map_err(native_error)?;
        commit.entities = root;
        commit.entity_creation_nonces = root;
        Ok(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use reth_ethereum::evm::revm::{DatabaseCommit, database_interface::EmptyDB, db::CacheDB};

    #[test]
    fn live_view_replay_and_forks_select_roots_from_ethereum() -> eyre::Result<()> {
        let records = Store::default();
        let mut native = CacheDB::new(EmptyDB::default());
        let e = Entity {
            key: [3; 32],
            owner: [7; 20],
            expires_at: 100,
            payload: b"genesis".to_vec(),
            ..Default::default()
        };
        let mut view = chain_manager(&mut native, BlockRef::new(0, [0; 32]), records.clone())?;
        view.update_entity(EntityUpdates::create(e.clone()))?;
        assert!(view.get_entity(e.key, ReadMode::ViewOnBase)?.is_none());
        assert_eq!(
            view.get_entity(e.key, ReadMode::ViewWithOverlay)?,
            Some(e.clone())
        );
        view.fetch_increment_entity_creation_nonce(e.owner)?;
        view.set_balance(
            e.owner,
            UserBalance::from_be_bytes(U256::from(123).to_be_bytes()),
        )?;
        view.fetch_increment_acc_nonce(e.owner)?;
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("{e:?}"))?;
        let diff = view.into_base().into_state();
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[&Address::from(e.owner)].info.nonce, 1);
        assert_eq!(diff[&Address::from(e.owner)].info.balance, U256::from(123));
        native.commit(diff);
        let parent = native.clone();
        let mut view = chain_manager(&mut native, BlockRef::new(1, [1; 32]), records.clone())?;
        let changed = Entity {
            payload: b"branch A".to_vec(),
            ..e.clone()
        };
        view.update_entity(EntityUpdates::create(changed.clone()))?;
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("{e:?}"))?;
        let a = view.into_base().into_state();
        let mut fork = parent.clone();
        let mut view = chain_manager(&mut fork, BlockRef::new(1, [1; 32]), records.clone())?;
        assert_eq!(
            view.get_entity(e.key, ReadMode::ViewOnBase)?,
            Some(e.clone())
        );
        view.update_entity(EntityUpdates::create(Entity {
            payload: b"branch B".to_vec(),
            ..e.clone()
        }))?;
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("{e:?}"))?;
        let b = view.into_base().into_state();
        assert_ne!(a[&ROOT_ACCOUNT].storage, b[&ROOT_ACCOUNT].storage);
        // Replay A independently from the same native parent and records.
        let mut replay = parent;
        let mut view = chain_manager(&mut replay, BlockRef::new(1, [1; 32]), records.clone())?;
        view.update_entity(EntityUpdates::create(changed))?;
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("{e:?}"))?;
        assert_eq!(a, view.into_base().into_state());
        // Selecting the original Ethereum parent still gives the original entity.
        let view = chain_manager(&mut replay, BlockRef::new(1, [1; 32]), records)?;
        assert_eq!(view.get_entity(e.key, ReadMode::ViewOnBase)?, Some(e));
        Ok(())
    }
}
