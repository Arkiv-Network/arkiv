//! [`GolemStateManager`] — one store, many views.
//!
//! This is what the `&self` [`Store`] seam bought: the manager holds an
//! [`Arc`] of the store and hands a clone to every view it mints, so several
//! views can be open over one store at once — which is what a host does to
//! validate competing payloads at the same height.
//!
//! # Blocks and commits
//!
//! reth addresses state by block hash; the store numbers commits. The bridge
//! is [`StoreExt::commit_tagged`]: [`seal`](GolemStateManager::seal) tags a
//! commit with the block hash, and [`view`](StateManager::view) resolves that
//! tag back. The mapping lives in the store rather than in a host-side map,
//! so it cannot disagree with retention after a crash.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use arkiv_interfaces::execution::Op;
use arkiv_interfaces::gas::{CostModel, PlaceholderCost};
use arkiv_interfaces::primitives::Gas;
use arkiv_interfaces::query::QueryStats;
use arkiv_interfaces::statemanager::{BlockRef, SessionId, StateManager};
use arkiv_interfaces::store::{CommitId, Store, StoreExt};

use crate::view::{GolemStateView, ViewError};

/// Mints [`GolemStateView`]s over one shared store.
#[derive(Debug)]
pub struct GolemStateManager<S: ?Sized, C = PlaceholderCost> {
    store: Arc<S>,
    costs: C,
    /// Session ids are a plain counter.
    ///
    /// ponytail: unique within a process, which is all a [`SessionId`] is for
    /// today — make it random if one ever has to be unique across nodes.
    sessions: AtomicU64,
}

impl<S: StoreExt + ?Sized> GolemStateManager<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self::with_cost_model(store, PlaceholderCost)
    }
}

impl<S: StoreExt + ?Sized, C> GolemStateManager<S, C> {
    pub const fn with_cost_model(store: Arc<S>, costs: C) -> Self {
        Self {
            store,
            costs,
            sessions: AtomicU64::new(0),
        }
    }

    pub const fn cost_model(&self) -> &C {
        &self.costs
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    fn next_session(&self) -> SessionId {
        let mut bytes = [0u8; 16];
        bytes[8..].copy_from_slice(&self.sessions.fetch_add(1, Ordering::Relaxed).to_be_bytes());
        SessionId(bytes)
    }

    /// Promote a view's branch to the commit for `block`, tagged with its hash
    /// so [`StateManager::view`] can find it again.
    ///
    /// The counterpart of [`view`](StateManager::view): without this nothing
    /// produces the tags that one reads.
    pub fn seal(
        &self,
        view: GolemStateView<Arc<S>>,
        block: BlockRef,
    ) -> Result<CommitId, ViewError> {
        let branch = view.branch();
        drop(view);
        Ok(self.store.commit_tagged(branch, block.hash)?)
    }
}

impl<S: StoreExt + ?Sized, C: CostModel> StateManager for GolemStateManager<S, C> {
    type Error = ViewError;
    type View = GolemStateView<Arc<S>>;

    fn view(&self, at: BlockRef) -> Result<Self::View, ViewError> {
        let origin = self
            .store
            .commit_by_tag(at.hash)?
            .ok_or(ViewError::UnknownBlock)?;
        let branch = self.store.begin(Some(origin))?;
        Ok(GolemStateView::new(
            Arc::clone(&self.store),
            branch,
            origin,
            at,
            self.next_session(),
        ))
    }

    fn get_operation_cost(&mut self, op: &Op) -> Result<Gas, ViewError> {
        Ok(self.costs.op_cost(op))
    }

    fn get_query_cost(&mut self, stats: &QueryStats) -> Result<Gas, ViewError> {
        Ok(self.costs.query_cost(stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::Entity;
    use arkiv_interfaces::statemanager::{
        EntityStore, EntityUpdates, ReadMode, StateView, StoreKind,
    };
    use arkiv_interfaces::store::reference::MemStore;

    const GENESIS: BlockRef = BlockRef {
        height: 0,
        hash: [0; 32],
    };
    const ONE: BlockRef = BlockRef {
        height: 1,
        hash: [1; 32],
    };

    /// A manager whose genesis commit is tagged, so `view` has somewhere to start.
    ///
    /// `MemStore` is a `RefCell`, so this `Arc` is not `Send + Sync`. That is
    /// the reference store's limitation, not the manager's — `arkiv-valkey`
    /// pins the real store as shareable.
    #[allow(clippy::arc_with_non_send_sync)]
    fn manager() -> GolemStateManager<MemStore> {
        let store = Arc::new(MemStore::new());
        let branch = store.begin(None).expect("begin");
        store
            .commit_tagged(branch, GENESIS.hash)
            .expect("tag genesis");
        GolemStateManager::new(store)
    }

    fn entity(key: u8) -> Entity {
        Entity {
            key: [key; 32],
            ..Entity::default()
        }
    }

    #[test]
    fn a_sealed_view_is_what_the_next_view_reads() {
        let manager = manager();

        let mut view = manager.view(GENESIS).expect("view at genesis");
        view.update_entity(EntityUpdates::create(entity(1)))
            .expect("create");
        StateView::commit(&mut view).expect("commit");
        view.graduate(ONE).expect("graduate");

        let mut view = manager.view(GENESIS).expect("view at genesis");
        view.update_entity(EntityUpdates::create(entity(1)))
            .expect("create");
        StateView::commit(&mut view).expect("commit");
        manager.seal(view, ONE).expect("seal");

        let next = manager.view(ONE).expect("view at block one");
        assert_eq!(
            next.get_entity([1; 32], ReadMode::ViewOnBase)
                .expect("read"),
            Some(entity(1)),
        );
        assert!(next.has_store(StoreKind::Entities));
    }

    /// The property the whole `&self` seam exists for.
    #[test]
    fn two_views_over_one_store_are_independent() {
        let manager = manager();

        let mut a = manager.view(GENESIS).expect("view a");
        let mut b = manager.view(GENESIS).expect("view b");
        assert_ne!(a.session(), b.session());

        a.update_entity(EntityUpdates::create(entity(1)))
            .expect("create in a");
        b.update_entity(EntityUpdates::create(entity(2)))
            .expect("create in b");

        assert!(
            a.get_entity([2; 32], ReadMode::ViewWithOverlay)
                .expect("read")
                .is_none(),
            "one view must not see another's staged writes",
        );
        assert!(
            b.get_entity([1; 32], ReadMode::ViewWithOverlay)
                .expect("read")
                .is_none(),
        );
    }

    #[test]
    fn a_block_the_store_never_committed_is_not_a_view() {
        assert!(matches!(manager().view(ONE), Err(ViewError::UnknownBlock)));
    }
}
