//! [`MptStateView`] — the spec's `StateView` over two backends: the Ethereum
//! account state (balances, transaction nonces, the anchor slot) through a
//! base `B`, and the Arkiv database (entities, minting nonces, the index)
//! through a node store `N`. Per-store staging maps sit over both, flushed
//! through on commit. Base reads bypass the staging, which is all
//! `ViewOnBase` is.
//!
//! A commit of the database side writes new trie nodes into `N` and the new
//! database root into the anchor slot through `B`, so the Ethereum diff a
//! transaction returns carries exactly one word of Arkiv state: its root.

use core::cell::RefCell;
use core::mem;
use core::ops::Bound;
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::BTreeMap;

use alloy_primitives::B256;
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
use arkiv_reth_uncommitted_store::MemPruningStore;
use arkiv_store::{
    AttrEntry, DbChanges, DbRoots, DbView, EntityDelta, NodeReader, NodeSink, StoreError,
    annotation_delta, commit_changes,
};

use crate::seams::{
    AnchorAccess, BalanceAccess, NonceAccess, RethAccountBalancesStore, RethAccountNoncesStore,
};

/// What can go wrong in an [`MptStateView`]: `E` is the base's error, `S` the
/// node store's.
#[derive(Debug)]
pub enum MptError<E, S> {
    Backend(E),
    Store(StoreError<S>),
    /// The operation isn't available on this host.
    Unsupported(&'static str),
    /// The view cannot graduate — see [`StateView::graduate`].
    Graduate(&'static str),
}

impl<E, S> From<StoreError<S>> for MptError<E, S> {
    fn from(e: StoreError<S>) -> Self {
        Self::Store(e)
    }
}

fn next_session() -> SessionId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&std::process::id().to_be_bytes());
    bytes[8..].copy_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    SessionId(bytes)
}

#[derive(Debug)]
pub struct MptStateView<B, N, C = PlaceholderCost> {
    // RefCell because the spec's reads are `&self` while every seam reads
    // with `&mut` (caches). Borrows never overlap: each method takes one.
    base: RefCell<B>,
    nodes: RefCell<N>,
    /// The database roots this view reads as its base.
    roots: DbRoots,
    costs: C,
    block: BlockRef,
    session: SessionId,
    entities: BTreeMap<EntityAddress, EntityUpdates>,
    index: BTreeMap<EntityAddress, EntityDelta>,
    balances: BTreeMap<UserAddress, UserBalance>,
    account_nonces: BTreeMap<UserAddress, UserNonce>,
    creation_nonces: BTreeMap<UserAddress, EntityCreationNonce>,
    pruning: MemPruningStore,
    /// The database root after the last commit.
    commitment: Option<Commitment>,
}

impl<B, N> MptStateView<B, N>
where
    B: AnchorAccess,
    N: NodeReader,
{
    /// Open a view: the base's anchor slot names the database root.
    pub fn new(base: B, nodes: N, block: BlockRef) -> Result<Self, MptError<B::Error, N::Error>> {
        Self::with_cost_model(base, nodes, block, PlaceholderCost)
    }
}

impl<B, N, C> MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader,
{
    pub fn with_cost_model(
        mut base: B,
        nodes: N,
        block: BlockRef,
        costs: C,
    ) -> Result<Self, MptError<B::Error, N::Error>> {
        let root = base.db_root().map_err(MptError::Backend)?;
        let roots = DbRoots::load(&nodes, root)
            .map_err(|e| MptError::Store(StoreError::Store(e)))?
            .ok_or(MptError::Store(StoreError::UnknownRoot(root)))?;
        Ok(Self {
            base: RefCell::new(base),
            nodes: RefCell::new(nodes),
            roots,
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
        })
    }
}

impl<B, N, C> MptStateView<B, N, C> {
    pub const fn cost_model(&self) -> &C {
        &self.costs
    }

    pub fn backend_mut(&mut self) -> &mut B {
        self.base.get_mut()
    }

    pub fn nodes_mut(&mut self) -> &mut N {
        self.nodes.get_mut()
    }

    /// The database roots this view reads as its base.
    pub const fn roots(&self) -> &DbRoots {
        &self.roots
    }

    /// The database root as of the last commit, or the base's.
    pub fn db_root(&self) -> B256 {
        self.commitment
            .map(B256::from)
            .unwrap_or_else(|| self.roots.root())
    }

    pub fn into_parts(self) -> (B, N) {
        (self.base.into_inner(), self.nodes.into_inner())
    }

    fn is_dirty(&self) -> bool {
        !self.entities.is_empty()
            || !self.index.is_empty()
            || !self.balances.is_empty()
            || !self.account_nonces.is_empty()
            || !self.creation_nonces.is_empty()
            || self.pruning.is_dirty()
    }

    /// Every committed store answers with the database root: the one
    /// commitment this host has. Before any commit, the base's root.
    fn shared_commitment(&self) -> Commitment {
        self.commitment.unwrap_or_else(|| self.roots.root().0)
    }
}

impl<B, N, C> MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader,
{
    fn base_entity(
        &self,
        address: EntityAddress,
    ) -> Result<Option<Entity>, MptError<B::Error, N::Error>> {
        let nodes = self.nodes.borrow();
        Ok(DbView::at(&*nodes, self.roots).entity(&address)?)
    }

    fn overlaid_entity(
        &self,
        updates: &EntityUpdates,
    ) -> Result<Option<Entity>, MptError<B::Error, N::Error>> {
        if updates.delete {
            return Ok(None);
        }
        let mut entity = self.base_entity(updates.entity)?.unwrap_or_default();
        updates.apply_to(&mut entity);
        Ok(Some(entity))
    }

    /// Derive each update's delta against the base and stage it, replacing any
    /// earlier delta for the same entity. Shared by both index stores.
    fn stage_index_deltas(
        &mut self,
        updates: &[EntityUpdates],
    ) -> Result<(), MptError<B::Error, N::Error>> {
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

impl<B, N, C> MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader + NodeSink,
{
    /// Flush every staged database write — entities, their index deltas and
    /// the minting nonces — as one commit: new nodes into the store, the new
    /// root into the anchor. The three `commit_store`s of the database-side
    /// stores all come here, so the second and third find nothing to do.
    fn flush_db(&mut self) -> Result<(), MptError<B::Error, N::Error>> {
        let staged_entities = mem::take(&mut self.entities);
        let staged_nonces = mem::take(&mut self.creation_nonces);
        // The index is derived from the entity changes at commit; the staged
        // deltas only served overlay reads.
        self.index.clear();
        if staged_entities.is_empty() && staged_nonces.is_empty() {
            // Nothing to write, but the view now counts as committed.
            self.commitment.get_or_insert(self.roots.root().0);
            return Ok(());
        }

        let mut changes = DbChanges::default();
        for (key, updates) in staged_entities {
            let after = self.overlaid_entity(&updates)?;
            changes.entities.insert(key, after);
        }
        changes.nonces = staged_nonces;

        let new_roots = commit_changes(self.nodes.get_mut(), self.roots, &changes)?;
        self.roots = new_roots;
        let root = new_roots.root();
        self.base
            .get_mut()
            .set_db_root(root)
            .map_err(MptError::Backend)?;
        self.commitment = Some(root.0);
        Ok(())
    }
}

// ── The committed stores ────────────────────────────────────────────────────

impl<B, N, C> EntityStore for MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader + NodeSink,
{
    type Error = MptError<B::Error, N::Error>;

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
        self.flush_db()?;
        Ok(self.shared_commitment())
    }
}

impl<B, N, C> EqualityIndexStore for MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader + NodeSink,
{
    type Error = MptError<B::Error, N::Error>;

    fn get_equal_entities(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        let keys = {
            let nodes = self.nodes.borrow();
            DbView::at(&*nodes, self.roots).equal(attribute, value)?
        };
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
        let mut keys = {
            let nodes = self.nodes.borrow();
            DbView::at(&*nodes, self.roots).prefixed(attribute, prefix)?
        };
        keys.sort_unstable();
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
        self.flush_db()
    }
}

impl<B, N, C> RangeIndexStore for MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader + NodeSink,
{
    type Error = MptError<B::Error, N::Error>;

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
            // Bounds of two types name disjoint indexes: nothing matches.
            (Some(a), Some(b)) if a.attr_type() != b.attr_type() => return Ok(Vec::new()),
            (Some(v), _) | (None, Some(v)) => v.attr_type(),
        };

        let mut keys = {
            let nodes = self.nodes.borrow();
            DbView::at(&*nodes, self.roots).range(attribute, ty, low, high)?
        };
        keys.sort_unstable();
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
        self.flush_db()
    }
}

impl<B, N, C> AccountBalancesStore for MptStateView<B, N, C>
where
    B: AnchorAccess + BalanceAccess<Error = <B as AnchorAccess>::Error>,
    N: NodeReader + NodeSink,
{
    type Error = MptError<<B as AnchorAccess>::Error, N::Error>;

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
        RethAccountBalancesStore(&mut *self.base.borrow_mut())
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
            RethAccountBalancesStore(self.base.get_mut())
                .set_balance(account, balance)
                .map_err(MptError::Backend)?;
        }
        Ok(self.shared_commitment())
    }
}

impl<B, N, C> AccountNoncesStore for MptStateView<B, N, C>
where
    B: AnchorAccess + NonceAccess<Error = <B as AnchorAccess>::Error>,
    N: NodeReader + NodeSink,
{
    type Error = MptError<<B as AnchorAccess>::Error, N::Error>;

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
        RethAccountNoncesStore(&mut *self.base.borrow_mut())
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
            RethAccountNoncesStore(self.base.get_mut())
                .set_account_nonce(account, nonce)
                .map_err(MptError::Backend)?;
        }
        Ok(self.shared_commitment())
    }
}

impl<B, N, C> EntityCreationNoncesStore for MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader + NodeSink,
{
    type Error = MptError<B::Error, N::Error>;

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
        let nodes = self.nodes.borrow();
        Ok(DbView::at(&*nodes, self.roots).creation_nonce(&owner)?)
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
        self.flush_db()?;
        Ok(self.shared_commitment())
    }
}

// ── The uncommitted store ───────────────────────────────────────────────────

impl<B, N, C> PruningStore for MptStateView<B, N, C>
where
    B: AnchorAccess,
    N: NodeReader + NodeSink,
{
    type Error = MptError<B::Error, N::Error>;

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

impl<B, N, C> StateView for MptStateView<B, N, C>
where
    B: AnchorAccess
        + BalanceAccess<Error = <B as AnchorAccess>::Error>
        + NonceAccess<Error = <B as AnchorAccess>::Error>,
    N: NodeReader + NodeSink,
{
    type Error = MptError<<B as AnchorAccess>::Error, N::Error>;

    fn base(&self) -> BlockRef {
        self.block
    }

    fn session(&self) -> SessionId {
        self.session
    }

    /// Always `true`: every root this node has ever written stays readable.
    fn has_store(&self, _store: StoreKind) -> bool {
        true
    }

    fn graduate(self, block: BlockRef) -> Result<StateCommit, <Self as StateView>::Error> {
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
    use alloy_primitives::{Address, U256};
    use arkiv_interfaces::entity::annotations::OWNER;
    use arkiv_store::SharedMemNodeStore;

    use crate::testing::{MemBase, MemView, mem_view, settle};

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

    fn stage_create(view: &mut MemView<'_>, byte: u8, owner: u8) {
        view.entities_mut()
            .update_entity(EntityUpdates::create(entity_of(byte, owner)))
            .unwrap();
        let deltas = view.entities().get_uncommitted_deltas().unwrap();
        view.equality_index_mut().apply_deltas(&deltas).unwrap();
        view.range_index_mut().apply_deltas(&deltas).unwrap();
    }

    fn owned_by(view: &MemView<'_>, owner: u8, read: ReadMode) -> Vec<EntityAddress> {
        view.equality_index()
            .get_equal_entities(OWNER, &AttributeValue::EthereumAddress([owner; 20]), read)
            .unwrap()
    }

    fn block() -> BlockRef {
        BlockRef::new(10, [0xBB; 32])
    }

    #[test]
    fn overlay_writes_stay_off_the_base_until_commit() {
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
        let key = key_of(7);
        view.update_entity(EntityUpdates::create(entity_of(7, 2)))
            .unwrap();

        assert_eq!(
            view.get_entity(key, ReadMode::ViewWithOverlay).unwrap(),
            Some(entity_of(7, 2))
        );
        assert_eq!(view.get_entity(key, ReadMode::ViewOnBase).unwrap(), None);
        assert_eq!(view.db_root(), B256::ZERO);

        StateView::commit(&mut view).unwrap();
        assert_eq!(
            view.get_entity(key, ReadMode::ViewOnBase).unwrap(),
            Some(entity_of(7, 2))
        );
        // The anchor now names the new root, and the nodes are staged.
        let root = view.db_root();
        assert_ne!(root, B256::ZERO);
        assert_eq!(view.backend_mut().db_root, root);

        // A fresh view over the settled base reads the same entity.
        let base = settle(view, &nodes);
        let next = mem_view(base, &nodes, block());
        assert_eq!(
            next.get_entity(key, ReadMode::ViewOnBase).unwrap(),
            Some(entity_of(7, 2))
        );
        assert_eq!(next.db_root(), root);
    }

    #[test]
    fn partial_updates_patch_only_what_they_set() {
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
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
        assert_eq!(view.get_uncommitted_deltas().unwrap().len(), 1);
    }

    #[test]
    fn index_reads_honor_read_mode() {
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
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
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
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
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
        stage_create(&mut view, 0xA0, 1);
        StateView::commit(&mut view).unwrap();

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
        assert_eq!(owned_by(&view, 1, ReadMode::ViewOnBase), vec![key_of(0xA0)]);
    }

    #[test]
    fn balances_and_nonces_stage_then_flush() {
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
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
        // Nothing touched the database: the anchor stays empty.
        assert_eq!(view.backend_mut().db_root, B256::ZERO);
    }

    #[test]
    fn creation_nonces_stage_then_land_in_the_database() {
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
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
        assert_ne!(view.backend_mut().db_root, B256::ZERO);
        let base = settle(view, &nodes);
        let next = mem_view(base, &nodes, block());
        assert_eq!(
            next.get_entity_creation_nonce(alice, ReadMode::ViewOnBase)
                .unwrap(),
            EntityCreationNonce::new(2)
        );
    }

    #[test]
    fn graduate_needs_a_committed_view_extending_the_base() {
        let nodes = SharedMemNodeStore::new();
        let mut dirty = mem_view(MemBase::default(), &nodes, block());
        dirty
            .update_entity(EntityUpdates::create(entity_of(1, 1)))
            .unwrap();
        assert!(matches!(
            dirty.graduate(BlockRef::new(11, [0x11; 32])),
            Err(MptError::Graduate(_))
        ));

        let uncommitted = mem_view(MemBase::default(), &nodes, block());
        assert!(matches!(
            uncommitted.graduate(BlockRef::new(11, [0x11; 32])),
            Err(MptError::Graduate(_))
        ));

        let mut committed = mem_view(MemBase::default(), &nodes, block());
        stage_create(&mut committed, 1, 1);
        StateView::commit(&mut committed).unwrap();
        let root = committed.db_root();
        let base = settle(committed, &nodes);

        let wrong_height = mem_view(base.clone(), &nodes, block());
        assert!(matches!(
            wrong_height.graduate(BlockRef::new(13, [0x13; 32])),
            Err(MptError::Graduate(_))
        ));

        let mut committed = mem_view(base, &nodes, block());
        StateView::commit(&mut committed).unwrap();
        let commit = committed.graduate(BlockRef::new(11, [0x11; 32])).unwrap();
        assert_eq!(commit.parent, BlockRef::new(10, [0xBB; 32]));
        assert_eq!(commit.block.height, 11);
        assert_eq!(commit.entities, root.0);
        assert_eq!(commit.entities, commit.balances);
        assert_eq!(commit.entities, commit.account_nonces);
        assert_eq!(commit.entities, commit.entity_creation_nonces);
    }

    #[test]
    fn sessions_are_unique_per_view() {
        let nodes = SharedMemNodeStore::new();
        let a = mem_view(MemBase::default(), &nodes, block());
        let b = mem_view(MemBase::default(), &nodes, block());
        assert_ne!(a.session(), b.session());
        assert_eq!(a.base(), b.base());
    }

    #[test]
    fn deletes_tombstone_and_unindex() {
        let nodes = SharedMemNodeStore::new();
        let mut view = mem_view(MemBase::default(), &nodes, block());
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
        // Every entity gone: the database is empty again.
        assert_eq!(view.db_root(), B256::ZERO);
    }
}
