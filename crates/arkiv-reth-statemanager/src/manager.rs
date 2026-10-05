//! [`MptStateView`] — the spec's `StateView` over Ethereum's one MPT account
//! state: per-store staging maps over a base `B`, flushed through on commit.
//! Base reads bypass the staging, which is all `ViewOnBase` is.

use core::cell::RefCell;
use core::mem;
use core::ops::Bound;
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::BTreeMap;

use arkiv_interfaces::entity::{AttributeValue, Entity};
use arkiv_interfaces::gas::PlaceholderCost;
use arkiv_interfaces::primitives::{
    EntityAddress, EntityCreationNonce, UserAddress, UserBalance, UserNonce,
};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, AccountNoncesStore, BlockRef, Commitment, EntityCreationNoncesStore,
    EntityStore, EntityUpdates, EqualityIndexStore, PruningMeta, PruningStore, RangeIndexStore,
    ReadMode, SessionId, StateCommit, StateView, StoreKind,
};
use arkiv_reth_mpt_committed_store::{
    AccountCode, AttrEntry, AuxError, AuxiliaryEntityDelta, BalanceAccess, CodeBackend,
    CodeBackendError, IndexStorage, NonceAccess, RethAccountBalancesStore, RethAccountNoncesStore,
    RethAuxStore, RethEntityCreationNoncesStore, RethEntityStore, annotation_delta,
};
use arkiv_reth_uncommitted_store::MemPruningStore;

/// What can go wrong in an [`MptStateView`], generic over the base's error `E`.
#[derive(Debug)]
pub enum MptError<E> {
    Backend(E),
    Entity(CodeBackendError<E>),
    Index(AuxError<E>),
    /// The operation isn't available on this host.
    Unsupported(&'static str),
    /// The view cannot graduate — see [`StateView::graduate`].
    Graduate(&'static str),
}

fn next_session() -> SessionId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&std::process::id().to_be_bytes());
    bytes[8..].copy_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    SessionId(bytes)
}

#[derive(Debug)]
pub struct MptStateView<B, C = PlaceholderCost> {
    // RefCell because the spec's reads are `&self` while every reth seam reads
    // with `&mut` (caches). Borrows never overlap: each method takes one.
    base: RefCell<B>,
    costs: C,
    block: BlockRef,
    session: SessionId,
    entities: BTreeMap<EntityAddress, EntityUpdates>,
    index: BTreeMap<EntityAddress, AuxiliaryEntityDelta>,
    balances: BTreeMap<UserAddress, UserBalance>,
    account_nonces: BTreeMap<UserAddress, UserNonce>,
    creation_nonces: BTreeMap<UserAddress, EntityCreationNonce>,
    pruning: MemPruningStore,
    commitment: Option<Commitment>,
}

impl<B> MptStateView<B> {
    pub fn new(base: B, block: BlockRef) -> Self {
        Self::with_cost_model(base, block, PlaceholderCost)
    }
}

impl<B, C> MptStateView<B, C> {
    pub fn with_cost_model(base: B, block: BlockRef, costs: C) -> Self {
        Self {
            base: RefCell::new(base),
            costs,
            block,
            session: next_session(),
            entities: BTreeMap::new(),
            index: BTreeMap::new(),
            balances: BTreeMap::new(),
            account_nonces: BTreeMap::new(),
            creation_nonces: BTreeMap::new(),
            pruning: MemPruningStore::new(),
            commitment: None,
        }
    }

    pub const fn cost_model(&self) -> &C {
        &self.costs
    }

    pub fn backend_mut(&mut self) -> &mut B {
        self.base.get_mut()
    }

    pub fn into_base(self) -> B {
        self.base.into_inner()
    }

    fn is_dirty(&self) -> bool {
        !self.entities.is_empty()
            || !self.index.is_empty()
            || !self.balances.is_empty()
            || !self.account_nonces.is_empty()
            || !self.creation_nonces.is_empty()
            || self.pruning.is_dirty()
    }

    /// All store commitments are the same on this host: computed once, cached.
    /// Zero stands in for the unified state root reth computes at seal time.
    fn shared_commitment(&mut self) -> Commitment {
        *self.commitment.get_or_insert_with(Commitment::default)
    }
}

impl<B, C, E> MptStateView<B, C>
where
    B: AccountCode<Error = E>,
    E: core::fmt::Debug,
{
    fn base_entity(&self, address: EntityAddress) -> Result<Option<Entity>, MptError<E>> {
        RethEntityStore::new(CodeBackend::new(&mut *self.base.borrow_mut()))
            .get(address)
            .map_err(MptError::Entity)
    }

    fn overlaid_entity(&self, updates: &EntityUpdates) -> Result<Option<Entity>, MptError<E>> {
        if updates.delete {
            return Ok(None);
        }
        let mut entity = self.base_entity(updates.entity)?.unwrap_or_default();
        updates.apply_to(&mut entity);
        Ok(Some(entity))
    }
}

impl<B, C, E> MptStateView<B, C>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    /// Derive each update's delta against the base and stage it, replacing any
    /// earlier delta for the same entity. Shared by both index stores.
    fn stage_index_deltas(&mut self, updates: &[EntityUpdates]) -> Result<(), MptError<E>> {
        for u in updates {
            let before = self.base_entity(u.entity)?;
            let after = self.overlaid_entity(u)?;
            match annotation_delta(u.entity, before.as_ref(), after.as_ref()) {
                Some(delta) => self.index.insert(u.entity, delta),
                None => self.index.remove(&u.entity),
            };
        }
        Ok(())
    }

    /// Drains the staged map, so both index stores' `commit_store` share it and
    /// the second call is a no-op.
    fn flush_index(&mut self) -> Result<(), MptError<E>> {
        let staged: Vec<AuxiliaryEntityDelta> = mem::take(&mut self.index).into_values().collect();
        if staged.is_empty() {
            return Ok(());
        }
        RethAuxStore::new(self.base.get_mut())
            .apply_delta(&staged)
            .map_err(MptError::Index)
    }

    fn adjust_with_staged(
        &self,
        mut keys: Vec<EntityAddress>,
        hits: impl Fn(&AttrEntry) -> bool,
    ) -> Vec<EntityAddress> {
        for (key, delta) in &self.index {
            if delta.removes.iter().any(&hits) {
                keys.retain(|k| k != key);
            }
            if delta.inserts.iter().any(&hits) {
                keys.push(*key);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}

// ── The committed stores ────────────────────────────────────────────────────

impl<B, C, E> EntityStore for MptStateView<B, C>
where
    B: AccountCode<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_entity(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Option<Entity>, Self::Error> {
        match read {
            ReadMode::ViewOnBase => self.base_entity(address),
            ReadMode::ViewWithOverlay => match self.entities.get(&address) {
                Some(updates) => self.overlaid_entity(updates),
                None => self.base_entity(address),
            },
        }
    }

    fn update_entity(&mut self, updates: EntityUpdates) -> Result<(), Self::Error> {
        match self.entities.get_mut(&updates.entity) {
            // A delete on either side resets the merge: the newer update is
            // already the net change (a tombstone, or a full re-create).
            Some(staged) if !staged.delete && !updates.delete => {
                let entity = staged.entity;
                let mut merged = mem::take(staged);
                merged.entity = entity;
                if updates.creator.is_some() {
                    merged.creator = updates.creator;
                }
                if updates.owner.is_some() {
                    merged.owner = updates.owner;
                }
                if updates.created_at_block.is_some() {
                    merged.created_at_block = updates.created_at_block;
                }
                if updates.last_modified_at_block.is_some() {
                    merged.last_modified_at_block = updates.last_modified_at_block;
                }
                if updates.expires_at.is_some() {
                    merged.expires_at = updates.expires_at;
                }
                if updates.creation_flags.is_some() {
                    merged.creation_flags = updates.creation_flags;
                }
                if updates.content_type.is_some() {
                    merged.content_type = updates.content_type;
                }
                if updates.payload.is_some() {
                    merged.payload = updates.payload;
                }
                if updates.attributes.is_some() {
                    merged.attributes = updates.attributes;
                }
                *staged = merged;
            }
            _ => {
                self.entities.insert(updates.entity, updates);
            }
        }
        Ok(())
    }

    fn get_uncommitted_deltas(&self) -> Result<Vec<EntityUpdates>, Self::Error> {
        Ok(self.entities.values().cloned().collect())
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        let staged = mem::take(&mut self.entities);
        {
            let mut store = RethEntityStore::new(CodeBackend::new(self.base.get_mut()));
            for (key, updates) in staged {
                if updates.delete {
                    store.remove(key).map_err(MptError::Entity)?;
                } else {
                    let mut entity = store
                        .get(key)
                        .map_err(MptError::Entity)?
                        .unwrap_or_default();
                    updates.apply_to(&mut entity);
                    store.put(&entity).map_err(MptError::Entity)?;
                }
            }
        }
        Ok(self.shared_commitment())
    }
}

impl<B, C, E> EqualityIndexStore for MptStateView<B, C>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_equal_entities(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        let keys = RethAuxStore::new(&mut *self.base.borrow_mut())
            .equal_entities(attribute, value)
            .map_err(MptError::Index)?;
        Ok(match read {
            ReadMode::ViewOnBase => keys,
            ReadMode::ViewWithOverlay => {
                self.adjust_with_staged(keys, |e| e.attr == attribute && e.value == *value)
            }
        })
    }

    fn get_prefixed_entities(
        &self,
        attribute: &[u8],
        prefix: &str,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        let keys = RethAuxStore::new(&mut *self.base.borrow_mut())
            .prefixed_entities(attribute, prefix)
            .map_err(MptError::Index)?;
        Ok(match read {
            ReadMode::ViewOnBase => keys,
            ReadMode::ViewWithOverlay => self.adjust_with_staged(keys, |e| {
                e.attr == attribute
                    && matches!(&e.value, AttributeValue::Str(s) if s.as_bytes().starts_with(prefix.as_bytes()))
            }),
        })
    }

    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), Self::Error> {
        self.stage_index_deltas(entity_updates)
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        self.flush_index()
    }
}

impl<B, C, E> RangeIndexStore for MptStateView<B, C>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_within_range(
        &self,
        attribute: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        fn bound_value(b: Bound<&AttributeValue>) -> Option<&AttributeValue> {
            match b {
                Bound::Included(v) | Bound::Excluded(v) => Some(v),
                Bound::Unbounded => None,
            }
        }
        let ty = match (bound_value(low), bound_value(high)) {
            (None, None) => {
                return Err(MptError::Unsupported(
                    "a range lookup needs at least one typed bound",
                ));
            }
            // Bounds of two types name disjoint bucket sets: nothing matches.
            (Some(a), Some(b)) if a.attr_type() != b.attr_type() => return Ok(Vec::new()),
            (Some(v), _) | (None, Some(v)) => v.attr_type(),
        };

        let keys = RethAuxStore::new(&mut *self.base.borrow_mut())
            .entities_in_range(attribute, low, high)
            .map_err(MptError::Index)?;
        match read {
            ReadMode::ViewOnBase => Ok(keys),
            ReadMode::ViewWithOverlay => {
                // index_bytes ordering is numeric ordering for the range types.
                let above_low = |bytes: &[u8]| match low {
                    Bound::Included(v) => bytes >= v.index_bytes().as_slice(),
                    Bound::Excluded(v) => bytes > v.index_bytes().as_slice(),
                    Bound::Unbounded => true,
                };
                let below_high = |bytes: &[u8]| match high {
                    Bound::Included(v) => bytes <= v.index_bytes().as_slice(),
                    Bound::Excluded(v) => bytes < v.index_bytes().as_slice(),
                    Bound::Unbounded => true,
                };
                Ok(self.adjust_with_staged(keys, |e| {
                    e.attr == attribute && e.value.attr_type() == ty && {
                        let bytes = e.value.index_bytes();
                        above_low(&bytes) && below_high(&bytes)
                    }
                }))
            }
        }
    }

    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), Self::Error> {
        self.stage_index_deltas(entity_updates)
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        self.flush_index()
    }
}

impl<B, C, E> AccountBalancesStore for MptStateView<B, C>
where
    B: BalanceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_balance(
        &self,
        account: UserAddress,
        read: ReadMode,
    ) -> Result<UserBalance, Self::Error> {
        if read == ReadMode::ViewWithOverlay
            && let Some(balance) = self.balances.get(&account)
        {
            return Ok(*balance);
        }
        RethAccountBalancesStore::new(&mut *self.base.borrow_mut())
            .get_balance(account)
            .map_err(MptError::Backend)
    }

    fn fetch_add_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, Self::Error> {
        let current = self.get_balance(account, ReadMode::ViewWithOverlay)?;
        self.balances
            .insert(account, current.saturating_add(amount));
        Ok(current)
    }

    fn fetch_sub_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, Self::Error> {
        let current = self.get_balance(account, ReadMode::ViewWithOverlay)?;
        self.balances
            .insert(account, current.saturating_sub(amount));
        Ok(current)
    }

    fn compare_set_balance(
        &mut self,
        account: UserAddress,
        current: UserBalance,
        new: UserBalance,
    ) -> Result<bool, Self::Error> {
        if self.get_balance(account, ReadMode::ViewWithOverlay)? != current {
            return Ok(false);
        }
        self.balances.insert(account, new);
        Ok(true)
    }

    fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), Self::Error> {
        self.balances.insert(account, balance);
        Ok(())
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        let staged = mem::take(&mut self.balances);
        for (account, balance) in staged {
            RethAccountBalancesStore::new(self.base.get_mut())
                .set_balance(account, balance)
                .map_err(MptError::Backend)?;
        }
        Ok(self.shared_commitment())
    }
}

impl<B, C, E> AccountNoncesStore for MptStateView<B, C>
where
    B: NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_acc_nonce(
        &self,
        account: UserAddress,
        read: ReadMode,
    ) -> Result<UserNonce, Self::Error> {
        if read == ReadMode::ViewWithOverlay
            && let Some(nonce) = self.account_nonces.get(&account)
        {
            return Ok(*nonce);
        }
        RethAccountNoncesStore::new(&mut *self.base.borrow_mut())
            .get_account_nonce(account)
            .map_err(MptError::Backend)
    }

    fn fetch_increment_acc_nonce(
        &mut self,
        account: UserAddress,
    ) -> Result<UserNonce, Self::Error> {
        let current = self.get_acc_nonce(account, ReadMode::ViewWithOverlay)?;
        self.account_nonces.insert(account, current.next());
        Ok(current)
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        let staged = mem::take(&mut self.account_nonces);
        for (account, nonce) in staged {
            RethAccountNoncesStore::new(self.base.get_mut())
                .set_account_nonce(account, nonce)
                .map_err(MptError::Backend)?;
        }
        Ok(self.shared_commitment())
    }
}

impl<B, C, E> EntityCreationNoncesStore for MptStateView<B, C>
where
    B: IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_entity_creation_nonce(
        &self,
        owner: UserAddress,
        read: ReadMode,
    ) -> Result<EntityCreationNonce, Self::Error> {
        if read == ReadMode::ViewWithOverlay
            && let Some(nonce) = self.creation_nonces.get(&owner)
        {
            return Ok(*nonce);
        }
        RethEntityCreationNoncesStore::new(&mut *self.base.borrow_mut())
            .get_entity_nonce(owner)
            .map_err(MptError::Backend)
    }

    fn fetch_increment_entity_creation_nonce(
        &mut self,
        owner: UserAddress,
    ) -> Result<EntityCreationNonce, Self::Error> {
        let current = self.get_entity_creation_nonce(owner, ReadMode::ViewWithOverlay)?;
        self.creation_nonces.insert(owner, current.advanced_by(1));
        Ok(current)
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        let staged = mem::take(&mut self.creation_nonces);
        for (owner, nonce) in staged {
            RethEntityCreationNoncesStore::new(self.base.get_mut())
                .set_entity_nonce(owner, nonce)
                .map_err(MptError::Backend)?;
        }
        Ok(self.shared_commitment())
    }
}

// ── The uncommitted store ───────────────────────────────────────────────────

impl<B, C, E> PruningStore for MptStateView<B, C>
where
    B: BalanceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn add_to_pruning_set(
        &mut self,
        entity: EntityAddress,
        pruning_meta: PruningMeta,
    ) -> Result<(), Self::Error> {
        self.pruning.add_to_pruning_set(entity, pruning_meta);
        Ok(())
    }

    fn peek_top(
        &self,
        top: u16,
        read: ReadMode,
    ) -> Result<Vec<(EntityAddress, PruningMeta)>, Self::Error> {
        Ok(self.pruning.peek_top(top, read))
    }

    fn take_top(&mut self, top: u16) -> Result<Vec<(EntityAddress, PruningMeta)>, Self::Error> {
        Ok(self.pruning.take_top(top))
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        self.pruning.commit_store();
        Ok(())
    }
}

// ── The view ────────────────────────────────────────────────────────────────

impl<B, C, E> StateView for MptStateView<B, C>
where
    B: AccountCode<Error = E>
        + IndexStorage<Error = E>
        + BalanceAccess<Error = E>
        + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn base(&self) -> BlockRef {
        self.block
    }

    fn session(&self) -> SessionId {
        self.session
    }

    /// Always `true`: this host keeps every store in the one state trie.
    fn has_store(&self, _store: StoreKind) -> bool {
        true
    }

    fn graduate(self, block: BlockRef) -> Result<StateCommit, MptError<E>> {
        if self.is_dirty() {
            return Err(MptError::Graduate("staged writes remain uncommitted"));
        }
        let Some(commitment) = self.commitment else {
            return Err(MptError::Graduate("the view was never committed"));
        };
        if block.height != self.block.height + 1 {
            return Err(MptError::Graduate("the block does not extend the base"));
        }
        Ok(StateCommit {
            block,
            parent: self.block,
            entities: commitment,
            balances: commitment,
            account_nonces: commitment,
            entity_creation_nonces: commitment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use std::collections::{HashMap, HashSet};

    use alloy_primitives::{Address, B256, U256};
    use arkiv_interfaces::entity::annotations::OWNER;
    use arkiv_reth_mpt_committed_store::entities::layout::{
        SYSTEM_ACCOUNT_ADDRESS, entity_leaf_address, nonce_slot,
    };

    /// An in-memory base implementing every raw seam.
    #[derive(Debug, Default, Clone)]
    struct MemState {
        balances: HashMap<Address, U256>,
        nonces: HashMap<Address, u64>,
        code: HashMap<Address, Vec<u8>>,
        slots: HashMap<(Address, B256), B256>,
        persisted: HashSet<Address>,
    }

    impl AccountCode for MemState {
        type Error = Infallible;
        fn code(&mut self, addr: Address) -> Result<Vec<u8>, Infallible> {
            Ok(self.code.get(&addr).cloned().unwrap_or_default())
        }
        fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Infallible> {
            self.code.insert(addr, code);
            Ok(())
        }
        fn clear_code(&mut self, addr: Address) -> Result<(), Infallible> {
            self.code.remove(&addr);
            Ok(())
        }
    }

    impl IndexStorage for MemState {
        type Error = Infallible;
        fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Infallible> {
            Ok(self.slots.get(&(addr, slot)).copied().unwrap_or(B256::ZERO))
        }
        fn set_storage(
            &mut self,
            addr: Address,
            slot: B256,
            value: B256,
        ) -> Result<(), Infallible> {
            self.slots.insert((addr, slot), value);
            Ok(())
        }
        fn ensure_account_persists(&mut self, addr: Address) -> Result<(), Infallible> {
            self.persisted.insert(addr);
            Ok(())
        }
    }

    impl BalanceAccess for MemState {
        type Error = Infallible;
        fn get_balance(&mut self, addr: Address) -> Result<U256, Infallible> {
            Ok(self.balances.get(&addr).copied().unwrap_or_default())
        }
        fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Infallible> {
            self.balances.insert(addr, balance);
            Ok(())
        }
    }

    impl NonceAccess for MemState {
        type Error = Infallible;
        fn get_nonce(&mut self, addr: Address) -> Result<u64, Infallible> {
            Ok(self.nonces.get(&addr).copied().unwrap_or_default())
        }
        fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Infallible> {
            self.nonces.insert(addr, nonce);
            Ok(())
        }
    }

    type View = MptStateView<MemState>;

    fn view() -> View {
        View::new(MemState::default(), BlockRef::new(10, [0xBB; 32]))
    }

    /// The shared `StateView` contract, the same suite the GolemDB host runs.
    /// Two implementations that both pass cannot disagree about state.
    #[test]
    fn conformance() {
        arkiv_interfaces::statemanager::conformance::run_all(&view);
    }

    fn key_of(byte: u8) -> EntityAddress {
        [byte; 32]
    }

    fn entity_of(byte: u8, owner: u8) -> Entity {
        Entity {
            key: key_of(byte),
            owner: [owner; 20],
            expires_at: 100,
            payload: b"hi".to_vec(),
            ..Entity::default()
        }
    }

    /// Create + index an entity through the view's store accessors.
    fn stage_create(view: &mut View, byte: u8, owner: u8) {
        view.entities_mut()
            .update_entity(EntityUpdates::create(entity_of(byte, owner)))
            .unwrap();
        let deltas = view.entities().get_uncommitted_deltas().unwrap();
        view.equality_index_mut().apply_deltas(&deltas).unwrap();
        view.range_index_mut().apply_deltas(&deltas).unwrap();
    }

    fn owned_by(view: &View, owner: u8, read: ReadMode) -> Vec<EntityAddress> {
        view.equality_index()
            .get_equal_entities(OWNER, &AttributeValue::EthereumAddress([owner; 20]), read)
            .unwrap()
    }

    #[test]
    fn overlay_writes_stay_off_the_base_until_commit() {
        let mut view = view();
        let key = key_of(7);
        view.update_entity(EntityUpdates::create(entity_of(7, 2)))
            .unwrap();

        assert_eq!(
            view.get_entity(key, ReadMode::ViewWithOverlay).unwrap(),
            Some(entity_of(7, 2))
        );
        assert_eq!(view.get_entity(key, ReadMode::ViewOnBase).unwrap(), None);

        StateView::commit(&mut view).unwrap();
        assert_eq!(
            view.get_entity(key, ReadMode::ViewOnBase).unwrap(),
            Some(entity_of(7, 2))
        );
        // The entity really multiplexes onto account code.
        assert!(
            view.backend_mut()
                .code
                .contains_key(&entity_leaf_address(key))
        );
    }

    #[test]
    fn partial_updates_patch_only_what_they_set() {
        let mut view = view();
        view.update_entity(EntityUpdates::create(entity_of(7, 2)))
            .unwrap();
        StateView::commit(&mut view).unwrap();

        view.update_entity(EntityUpdates {
            entity: key_of(7),
            expires_at: Some(500),
            ..EntityUpdates::default()
        })
        .unwrap();

        let overlaid = view
            .get_entity(key_of(7), ReadMode::ViewWithOverlay)
            .unwrap()
            .unwrap();
        assert_eq!(overlaid.expires_at, 500);
        assert_eq!(overlaid.payload, b"hi", "unset fields read from the base");
        // Coalesced: one net delta for the entity.
        assert_eq!(view.get_uncommitted_deltas().unwrap().len(), 1);
    }

    #[test]
    fn index_reads_honor_read_mode() {
        let mut view = view();
        stage_create(&mut view, 0xA0, 1);

        assert_eq!(
            owned_by(&view, 1, ReadMode::ViewWithOverlay),
            vec![key_of(0xA0)]
        );
        assert!(owned_by(&view, 1, ReadMode::ViewOnBase).is_empty());

        StateView::commit(&mut view).unwrap();
        assert_eq!(owned_by(&view, 1, ReadMode::ViewOnBase), vec![key_of(0xA0)]);
        assert!(owned_by(&view, 9, ReadMode::ViewOnBase).is_empty());
    }

    #[test]
    fn range_and_prefix_lookups_answer_from_the_index() {
        let mut view = view();
        let mut with_attrs = entity_of(1, 1);
        with_attrs.attributes = vec![
            arkiv_interfaces::entity::Attribute::new(b"rank".to_vec(), AttributeValue::Int(5)),
            arkiv_interfaces::entity::Attribute::new(
                b"name".to_vec(),
                AttributeValue::Str("blue-team".into()),
            ),
        ];
        view.update_entity(EntityUpdates::create(with_attrs))
            .unwrap();
        let deltas = view.get_uncommitted_deltas().unwrap();
        EqualityIndexStore::apply_deltas(&mut view, &deltas).unwrap();
        StateView::commit(&mut view).unwrap();

        let low = AttributeValue::Int(1);
        let high = AttributeValue::Int(9);
        assert_eq!(
            view.get_within_range(
                b"rank",
                Bound::Included(&low),
                Bound::Included(&high),
                ReadMode::ViewOnBase,
            )
            .unwrap(),
            vec![key_of(1)]
        );
        assert!(
            view.get_within_range(
                b"rank",
                Bound::Excluded(&AttributeValue::Int(5)),
                Bound::Unbounded,
                ReadMode::ViewOnBase,
            )
            .unwrap()
            .is_empty()
        );
        // Bounds of two types match nothing; no bounds is refused.
        assert!(
            view.get_within_range(
                b"rank",
                Bound::Included(&AttributeValue::Int(1)),
                Bound::Included(&AttributeValue::U64(9)),
                ReadMode::ViewOnBase,
            )
            .unwrap()
            .is_empty()
        );
        assert!(matches!(
            view.get_within_range(
                b"rank",
                Bound::Unbounded,
                Bound::Unbounded,
                ReadMode::ViewOnBase
            ),
            Err(MptError::Unsupported(_))
        ));

        assert_eq!(
            view.get_prefixed_entities(b"name", "blue", ReadMode::ViewOnBase)
                .unwrap(),
            vec![key_of(1)]
        );
        assert!(
            view.get_prefixed_entities(b"name", "red", ReadMode::ViewOnBase)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn staged_index_deltas_adjust_overlay_reads() {
        let mut view = view();
        stage_create(&mut view, 0xA0, 1);
        StateView::commit(&mut view).unwrap();

        // Transfer: stage the owner swap, don't commit.
        view.update_entity(EntityUpdates {
            entity: key_of(0xA0),
            owner: Some([2; 20]),
            ..EntityUpdates::default()
        })
        .unwrap();
        let deltas = view.get_uncommitted_deltas().unwrap();
        EqualityIndexStore::apply_deltas(&mut view, &deltas).unwrap();

        assert!(owned_by(&view, 1, ReadMode::ViewWithOverlay).is_empty());
        assert_eq!(
            owned_by(&view, 2, ReadMode::ViewWithOverlay),
            vec![key_of(0xA0)]
        );
        // The base still answers the old owner until commit.
        assert_eq!(owned_by(&view, 1, ReadMode::ViewOnBase), vec![key_of(0xA0)]);
    }

    #[test]
    fn balances_and_nonces_stage_then_flush() {
        let mut view = view();
        let alice: UserAddress = [0xAA; 20];
        view.backend_mut()
            .balances
            .insert(Address::from(alice), U256::from(1_000u64));

        assert_eq!(
            view.fetch_sub_balance(alice, UserBalance::from_u64(300))
                .unwrap(),
            UserBalance::from_u64(1_000)
        );
        assert_eq!(
            view.get_balance(alice, ReadMode::ViewWithOverlay).unwrap(),
            UserBalance::from_u64(700)
        );
        assert_eq!(
            view.get_balance(alice, ReadMode::ViewOnBase).unwrap(),
            UserBalance::from_u64(1_000)
        );

        assert!(
            view.compare_set_balance(alice, UserBalance::from_u64(700), UserBalance::from_u64(50))
                .unwrap()
        );
        assert!(
            !view
                .compare_set_balance(alice, UserBalance::from_u64(700), UserBalance::ZERO)
                .unwrap()
        );

        assert_eq!(
            view.fetch_increment_acc_nonce(alice).unwrap(),
            UserNonce::ZERO
        );
        assert_eq!(
            view.get_acc_nonce(alice, ReadMode::ViewWithOverlay)
                .unwrap(),
            UserNonce::new(1)
        );

        StateView::commit(&mut view).unwrap();
        assert_eq!(
            view.backend_mut().balances[&Address::from(alice)],
            U256::from(50u64)
        );
        assert_eq!(view.backend_mut().nonces[&Address::from(alice)], 1);
    }

    #[test]
    fn creation_nonces_stage_then_land_on_the_system_account() {
        let mut view = view();
        let alice: UserAddress = [0xAA; 20];

        assert_eq!(
            view.fetch_increment_entity_creation_nonce(alice).unwrap(),
            EntityCreationNonce::ZERO
        );
        assert_eq!(
            view.fetch_increment_entity_creation_nonce(alice).unwrap(),
            EntityCreationNonce::new(1)
        );
        assert_eq!(
            view.get_entity_creation_nonce(alice, ReadMode::ViewOnBase)
                .unwrap(),
            EntityCreationNonce::ZERO
        );

        StateView::commit(&mut view).unwrap();
        let backend = view.backend_mut();
        assert!(backend.persisted.contains(&SYSTEM_ACCOUNT_ADDRESS));
        assert!(
            backend
                .slots
                .contains_key(&(SYSTEM_ACCOUNT_ADDRESS, nonce_slot(Address::from(alice))))
        );
    }

    #[test]
    fn graduate_needs_a_committed_view_extending_the_base() {
        let mut dirty = view();
        dirty
            .update_entity(EntityUpdates::create(entity_of(1, 1)))
            .unwrap();
        assert!(matches!(
            dirty.graduate(BlockRef::new(11, [0x11; 32])),
            Err(MptError::Graduate(_))
        ));

        let uncommitted = view();
        assert!(matches!(
            uncommitted.graduate(BlockRef::new(11, [0x11; 32])),
            Err(MptError::Graduate(_))
        ));

        let mut committed = view();
        stage_create(&mut committed, 1, 1);
        StateView::commit(&mut committed).unwrap();
        let wrong_height = MptStateView::new(
            committed.backend_mut().clone(),
            BlockRef::new(10, [0xBB; 32]),
        );
        assert!(matches!(
            wrong_height.graduate(BlockRef::new(13, [0x13; 32])),
            Err(MptError::Graduate(_))
        ));

        StateView::commit(&mut committed).unwrap();
        let commit = committed.graduate(BlockRef::new(11, [0x11; 32])).unwrap();
        assert_eq!(commit.parent, BlockRef::new(10, [0xBB; 32]));
        assert_eq!(commit.block.height, 11);
        // All commitments are the one shared value on this host.
        assert_eq!(commit.entities, commit.balances);
        assert_eq!(commit.entities, commit.account_nonces);
        assert_eq!(commit.entities, commit.entity_creation_nonces);
    }

    #[test]
    fn sessions_are_unique_per_view() {
        let a = view();
        let b = view();
        assert_ne!(a.session(), b.session());
        assert_eq!(a.base(), b.base());
    }

    #[test]
    fn deletes_tombstone_and_unindex() {
        let mut view = view();
        stage_create(&mut view, 0xA0, 1);
        StateView::commit(&mut view).unwrap();

        view.update_entity(EntityUpdates::deletion(key_of(0xA0)))
            .unwrap();
        let deltas = view.get_uncommitted_deltas().unwrap();
        EqualityIndexStore::apply_deltas(&mut view, &deltas).unwrap();

        assert_eq!(
            view.get_entity(key_of(0xA0), ReadMode::ViewWithOverlay)
                .unwrap(),
            None
        );
        assert!(owned_by(&view, 1, ReadMode::ViewWithOverlay).is_empty());

        StateView::commit(&mut view).unwrap();
        assert_eq!(
            view.get_entity(key_of(0xA0), ReadMode::ViewOnBase).unwrap(),
            None
        );
        assert!(owned_by(&view, 1, ReadMode::ViewOnBase).is_empty());
    }
}
