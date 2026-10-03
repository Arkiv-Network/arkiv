//! [`GolemStateView`] — Arkiv's `StateView` over one GolemDB [`Store`].
//!
//! Every store the view presents is the *same* store: entities are records,
//! accounts are records in the reserved key namespace, and the indexes are the
//! store's own query engine. So the view holds one handle, one branch to stage
//! on, and one commit to read the base from. The per-store logic lives in the
//! sibling modules, which implement their traits directly against this type.
//!
//! The one piece of genuinely view-local state is [`staged`](GolemStateView) —
//! the net-against-base delta per touched entity, which the trait promises and
//! the store does not keep. Writes themselves go straight to the branch.
//!
//! Pruning is node-local and carries no commitment, so it reuses
//! [`MemPruningStore`] unchanged rather than paying for store round-trips.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::statemanager::{
    BlockRef, Commitment, EntityUpdates, PruningMeta, PruningStore, ReadMode, SessionId,
    StateCommit, StateView, StoreKind,
};
use arkiv_interfaces::store::{BranchId, CommitId, ReadTarget, Store, StoreError};
use arkiv_reth_uncommitted_store::MemPruningStore;

use crate::accounts::AccountError;
use crate::entities::EntityError;
use crate::indices::{IndexError, StoreIndices};

/// Why an operation on a [`GolemStateView`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    Entity(EntityError),
    Account(AccountError),
    Index(IndexError),
    Store(StoreError),
    /// No commit carries that block's hash as its tag.
    UnknownBlock,
    /// A frame was merged or discarded without one being open.
    NoOpenFrame,
    /// The operation isn't available on this host.
    Unsupported(&'static str),
    /// The view cannot graduate — see [`StateView::graduate`].
    Graduate(&'static str),
}

impl From<EntityError> for ViewError {
    fn from(error: EntityError) -> Self {
        Self::Entity(error)
    }
}

impl From<AccountError> for ViewError {
    fn from(error: AccountError) -> Self {
        Self::Account(error)
    }
}

impl From<IndexError> for ViewError {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

impl From<StoreError> for ViewError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// One view over Arkiv's state, backed entirely by a GolemDB [`Store`].
#[derive(Debug)]
pub struct GolemStateView<S: Store> {
    pub(crate) store: S,
    /// Where this view's writes are staged.
    pub(crate) branch: BranchId,
    /// The commit `ViewOnBase` reads, and the one the indexes query.
    pub(crate) origin: CommitId,
    /// Net-against-base change per touched entity, ascending — the order
    /// [`EntityStore::get_uncommitted_deltas`] promises.
    pub(crate) staged: BTreeMap<EntityAddress, EntityUpdates>,
    pub(crate) pruning: MemPruningStore,
    /// Open transaction frames, innermost last: each entry is the branch and
    /// delta log to restore if its frame is discarded.
    frames: Vec<(BranchId, BTreeMap<EntityAddress, EntityUpdates>)>,
    block: BlockRef,
    session: SessionId,
    commitment: Option<Commitment>,
}

impl<S: Store> GolemStateView<S> {
    /// Stage on `branch` over the state committed at `origin`, as of `block`.
    pub const fn new(
        store: S,
        branch: BranchId,
        origin: CommitId,
        block: BlockRef,
        session: SessionId,
    ) -> Self {
        Self {
            store,
            branch,
            origin,
            staged: BTreeMap::new(),
            pruning: MemPruningStore::new(),
            frames: Vec::new(),
            block,
            session,
            commitment: None,
        }
    }

    /// Give the store back.
    pub fn into_store(self) -> S {
        self.store
    }

    pub const fn store(&self) -> &S {
        &self.store
    }

    /// The branch this view stages on — what a host commits to promote it.
    pub const fn branch(&self) -> BranchId {
        self.branch
    }

    pub(crate) const fn target(&self, read: ReadMode) -> ReadTarget {
        match read {
            ReadMode::ViewOnBase => ReadTarget::Commit(self.origin),
            ReadMode::ViewWithOverlay => ReadTarget::Branch(self.branch),
        }
    }

    /// Open a transaction frame: writes go to a child branch until the frame is
    /// merged or discarded.
    ///
    /// This is what makes a speculative execution safe. `eth_call`,
    /// `eth_estimateGas` and the engine tree's payload prewarming all reach the
    /// executor through the same entry point as a real transaction, and all of
    /// them throw the result away. Writing straight to the view's branch would
    /// let any one of them mutate chain state; inside a frame, discarding is
    /// exact — the child branch and its share of the delta log both go.
    ///
    /// Frames nest, as `Store::fork` does.
    pub fn begin_frame(&mut self) -> Result<(), ViewError> {
        let child = self.store.fork(self.branch)?;
        self.frames.push((self.branch, self.staged.clone()));
        self.branch = child;
        Ok(())
    }

    /// Fold the innermost frame into its parent; its writes become the parent's.
    pub fn merge_frame(&mut self) -> Result<(), ViewError> {
        let (parent, _) = self.frames.pop().ok_or(ViewError::NoOpenFrame)?;
        self.store.merge(self.branch)?;
        self.branch = parent;
        Ok(())
    }

    /// Drop the innermost frame. The view is left exactly as it was before the
    /// matching [`begin_frame`](Self::begin_frame) — no writes, no deltas.
    pub fn discard_frame(&mut self) -> Result<(), ViewError> {
        let (parent, staged) = self.frames.pop().ok_or(ViewError::NoOpenFrame)?;
        self.store.discard(self.branch)?;
        self.branch = parent;
        self.staged = staged;
        Ok(())
    }

    /// How many frames are open. Zero means writes land on the view's own branch.
    pub fn open_frames(&self) -> usize {
        self.frames.len()
    }

    /// The index lookups for this view, or a refusal if `read` asks for an
    /// overlay the store's query engine cannot give.
    pub(crate) fn indices(&self, read: ReadMode) -> Result<StoreIndices<'_, S>, ViewError> {
        if !crate::indices::supports(read) {
            return Err(ViewError::Unsupported(
                "index lookups answer from a commit, never from a branch",
            ));
        }
        Ok(StoreIndices::new(&self.store, self.origin))
    }

    /// The branch's content digest, read fresh.
    pub(crate) fn digest(&self) -> Result<Commitment, ViewError> {
        Ok(self.store.branch_digest(self.branch)?)
    }

    fn is_dirty(&self) -> bool {
        !self.staged.is_empty() || self.pruning.is_dirty() || !self.frames.is_empty()
    }

    /// Every store commits to the same value: the branch's content digest.
    /// Computed once per view, because it is one number for all of them.
    pub(crate) fn shared_commitment(&mut self) -> Result<Commitment, ViewError> {
        match self.commitment {
            Some(commitment) => Ok(commitment),
            None => {
                let commitment = self.digest()?;
                self.commitment = Some(commitment);
                Ok(commitment)
            }
        }
    }
}

impl<S: Store> PruningStore for GolemStateView<S> {
    type Error = ViewError;

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
    ) -> Result<alloc::vec::Vec<(EntityAddress, PruningMeta)>, Self::Error> {
        Ok(self.pruning.peek_top(top, read))
    }

    fn take_top(
        &mut self,
        top: u16,
    ) -> Result<alloc::vec::Vec<(EntityAddress, PruningMeta)>, Self::Error> {
        Ok(self.pruning.take_top(top))
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        self.pruning.commit_store();
        Ok(())
    }
}

impl<S: Store> StateView for GolemStateView<S> {
    type Error = ViewError;

    fn base(&self) -> BlockRef {
        self.block
    }

    fn session(&self) -> SessionId {
        self.session
    }

    /// One store backs all seven lanes, so either all are here or none is.
    fn has_store(&self, _store: StoreKind) -> bool {
        true
    }

    fn graduate(self, block: BlockRef) -> Result<StateCommit, <Self as StateView>::Error> {
        if self.is_dirty() {
            return Err(ViewError::Graduate("staged writes remain uncommitted"));
        }
        let Some(commitment) = self.commitment else {
            return Err(ViewError::Graduate("the view was never committed"));
        };
        if block.height != self.block.height + 1 {
            return Err(ViewError::Graduate("the block does not extend the base"));
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
pub(crate) mod tests {
    use super::*;
    use arkiv_interfaces::entity::Entity;
    use arkiv_interfaces::primitives::UserBalance;
    use arkiv_interfaces::statemanager::{
        AccountBalancesStore, AccountNoncesStore, EntityCreationNoncesStore, EntityStore,
    };
    use arkiv_interfaces::store::reference::MemStore;

    pub(crate) fn view() -> GolemStateView<MemStore> {
        let store = MemStore::new();
        let origin = store.head();
        let branch = store.begin(Some(origin)).unwrap();
        GolemStateView::new(
            store,
            branch,
            origin,
            BlockRef::new(10, [0xBB; 32]),
            SessionId([7; 16]),
        )
    }

    fn entity_of(key: u8) -> Entity {
        Entity {
            key: [key; 32],
            ..Entity::default()
        }
    }

    /// The shared `StateView` contract, the same suite the MPT host runs.
    #[test]
    fn conformance() {
        arkiv_interfaces::statemanager::conformance::run_all(&view);
    }

    /// The property that makes speculative execution safe: `eth_call` and
    /// friends run through the same entry point as a real transaction, so a
    /// discarded frame has to be indistinguishable from never having run.
    #[test]
    fn a_discarded_frame_leaves_no_trace() {
        let mut view = view();
        view.update_entity(EntityUpdates::create(entity_of(1)))
            .unwrap();
        let before = view.digest().unwrap();

        view.begin_frame().unwrap();
        view.update_entity(EntityUpdates::create(entity_of(2)))
            .unwrap();
        view.set_balance([0xaa; 20], UserBalance::from_u64(999))
            .unwrap();
        assert!(
            view.get_entity([2; 32], ReadMode::ViewWithOverlay)
                .unwrap()
                .is_some(),
            "the frame sees its own writes"
        );
        view.discard_frame().unwrap();

        assert_eq!(view.digest().unwrap(), before, "the digest is unmoved");
        assert_eq!(
            view.get_entity([2; 32], ReadMode::ViewWithOverlay).unwrap(),
            None,
            "the frame's entity is gone"
        );
        assert_eq!(
            view.get_balance([0xaa; 20], ReadMode::ViewWithOverlay)
                .unwrap(),
            UserBalance::from_u64(0),
            "and so is its account write"
        );
        assert_eq!(
            view.get_uncommitted_deltas().unwrap().len(),
            1,
            "the delta log is back to the one write outside the frame"
        );
    }

    #[test]
    fn a_merged_frame_keeps_its_writes() {
        let mut view = view();
        view.begin_frame().unwrap();
        view.update_entity(EntityUpdates::create(entity_of(2)))
            .unwrap();
        view.merge_frame().unwrap();

        assert_eq!(view.open_frames(), 0);
        assert_eq!(
            view.get_entity([2; 32], ReadMode::ViewWithOverlay).unwrap(),
            Some(entity_of(2)),
        );
        assert_eq!(view.get_uncommitted_deltas().unwrap().len(), 1);
    }

    #[test]
    fn frames_nest_and_an_inner_discard_spares_the_outer() {
        let mut view = view();
        view.begin_frame().unwrap();
        view.update_entity(EntityUpdates::create(entity_of(1)))
            .unwrap();

        view.begin_frame().unwrap();
        view.update_entity(EntityUpdates::create(entity_of(2)))
            .unwrap();
        assert_eq!(view.open_frames(), 2);
        view.discard_frame().unwrap();

        assert_eq!(
            view.get_entity([1; 32], ReadMode::ViewWithOverlay).unwrap(),
            Some(entity_of(1)),
            "the outer frame's write survives the inner discard"
        );
        assert_eq!(
            view.get_entity([2; 32], ReadMode::ViewWithOverlay).unwrap(),
            None,
        );
        view.merge_frame().unwrap();
        assert_eq!(view.open_frames(), 0);
    }

    #[test]
    fn a_view_with_an_open_frame_does_not_graduate() {
        let mut view = view();
        StateView::commit(&mut view).unwrap();
        view.begin_frame().unwrap();
        assert!(matches!(
            view.graduate(BlockRef::new(11, [0x11; 32])),
            Err(ViewError::Graduate(_)),
        ));
    }

    #[test]
    fn closing_a_frame_that_was_never_opened_is_an_error() {
        let mut view = view();
        assert!(matches!(view.merge_frame(), Err(ViewError::NoOpenFrame)));
        assert!(matches!(view.discard_frame(), Err(ViewError::NoOpenFrame)));
    }

    #[test]
    fn every_store_commits_to_the_same_digest() {
        let mut view = view();
        view.update_entity(EntityUpdates::create(entity_of(1)))
            .unwrap();
        view.set_balance([0xaa; 20], UserBalance::from_u64(5))
            .unwrap();

        let entities = EntityStore::commit_store(&mut view).unwrap();
        assert_eq!(
            AccountBalancesStore::commit_store(&mut view).unwrap(),
            entities
        );
        assert_eq!(
            AccountNoncesStore::commit_store(&mut view).unwrap(),
            entities
        );
        assert_eq!(
            EntityCreationNoncesStore::commit_store(&mut view).unwrap(),
            entities
        );
    }

    #[test]
    fn graduate_needs_a_committed_view_extending_the_base() {
        let mut dirty = view();
        dirty
            .update_entity(EntityUpdates::create(entity_of(1)))
            .unwrap();
        assert!(matches!(
            dirty.graduate(BlockRef::new(11, [0x11; 32])),
            Err(ViewError::Graduate(_))
        ));

        let uncommitted = view();
        assert!(matches!(
            uncommitted.graduate(BlockRef::new(11, [0x11; 32])),
            Err(ViewError::Graduate(_))
        ));

        let mut committed = view();
        committed
            .update_entity(EntityUpdates::create(entity_of(1)))
            .unwrap();
        StateView::commit(&mut committed).unwrap();
        assert!(matches!(
            committed.graduate(BlockRef::new(13, [0x13; 32])),
            Err(ViewError::Graduate(_))
        ));

        let mut ok = view();
        StateView::commit(&mut ok).unwrap();
        let commit = ok.graduate(BlockRef::new(11, [0x11; 32])).unwrap();
        assert_eq!(commit.parent, BlockRef::new(10, [0xBB; 32]));
        assert_eq!(commit.entities, commit.balances);
    }

    #[test]
    fn pruning_rides_along_in_memory() {
        let mut view = view();
        let meta = PruningMeta {
            priority: 1,
            introduced_at: 3,
        };
        view.add_to_pruning_set([1; 32], meta).unwrap();
        assert_eq!(
            view.peek_top(4, ReadMode::ViewWithOverlay).unwrap().len(),
            1
        );
        assert!(view.peek_top(4, ReadMode::ViewOnBase).unwrap().is_empty());
        PruningStore::commit_store(&mut view).unwrap();
        assert_eq!(view.peek_top(4, ReadMode::ViewOnBase).unwrap().len(), 1);
    }
}
