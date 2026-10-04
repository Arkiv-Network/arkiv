//! [`HostStateView`] — the reth host's `StateView`, on GolemDB.
//!
//! Entities, the query index, the pruning set and the entity-minting nonces
//! live in a GolemDB [`Store`](arkiv_interfaces::store::Store), reached through
//! [`GolemStateView`]. Balances and transaction nonces stay in reth's accounts,
//! staged into the [`EvmState`](crate::WriteOverlay) diff the block executor
//! commits.
//!
//! # Why accounts are still reth's, for now
//!
//! An Arkiv `UserBalance` *is* the Ethereum account balance, and a `UserNonce`
//! *is* the account nonce — the same values the txpool checks and
//! `eth_getBalance` returns, both of which read reth's state provider rather
//! than anything of ours. Until that provider is served from GolemDB, reth's
//! accounts have to keep moving or the node lies about balances.
//!
//! So this is a transition, and a bounded one: the two account lanes are the
//! only thing left on the reth side, and they collapse into
//! [`GolemStateView`]'s own `bal` and `non` cells — already written and tested
//! — the moment the provider lands.

use core::cell::RefCell;
use core::mem;
use core::ops::Bound;
use std::collections::BTreeMap;

use arkiv_golemdb_state::{GolemStateView, ViewError};
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
use arkiv_interfaces::store::{BranchId, CommitId, SealedCommit, Store, StoreExt};
use core::sync::atomic::{AtomicU64, Ordering};
use reth_ethereum::evm::primitives::Database;
use reth_ethereum::evm::revm::state::EvmState;
use std::sync::Mutex;

use crate::WriteOverlay;
use std::sync::Arc;

use crate::accounts::{BalanceAccess, NonceAccess};

/// The erased store handle the host carries. `StoreExt` is object-safe, a trait
/// object implements its supertrait `Store`, and `Arc<T: Store>` is a `Store` —
/// so nothing downstream of here has to be generic over the store type.
/// A node reaches its store from several threads, so the handle is `Send +
/// Sync`. The real store is both; the reference store is too, since it holds a
/// lock rather than a cell.
pub type HostStore = Arc<dyn StoreExt + Send + Sync>;

/// What can go wrong in a [`HostStateView`], over the account backend's error `E`.
#[derive(Debug)]
pub enum HostError<E> {
    /// The GolemDB side refused.
    Store(ViewError),
    /// The account backend refused.
    Backend(E),
    /// The view cannot graduate — see [`StateView::graduate`].
    Graduate(&'static str),
}

impl<E> From<ViewError> for HostError<E> {
    fn from(error: ViewError) -> Self {
        Self::Store(error)
    }
}

/// One view over Arkiv's state for one block: GolemDB for everything Arkiv
/// owns, reth's accounts for the two lanes Ethereum also reads.
#[derive(Debug)]
pub struct HostStateView<B, C = PlaceholderCost> {
    golem: GolemStateView<HostStore>,
    // RefCell because the spec's reads are `&self` while the reth account
    // seams read with `&mut` (caches). Borrows never overlap: one per method.
    base: RefCell<B>,
    costs: C,
    balances: BTreeMap<UserAddress, UserBalance>,
    account_nonces: BTreeMap<UserAddress, UserNonce>,
    commitment: Option<Commitment>,
}

impl<B> HostStateView<B> {
    pub fn new(golem: GolemStateView<HostStore>, base: B) -> Self {
        Self::with_cost_model(golem, base, PlaceholderCost)
    }
}

impl<B, C> HostStateView<B, C> {
    pub fn with_cost_model(golem: GolemStateView<HostStore>, base: B, costs: C) -> Self {
        Self {
            golem,
            base: RefCell::new(base),
            costs,
            balances: BTreeMap::new(),
            account_nonces: BTreeMap::new(),
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

    /// The GolemDB half, for a host that needs the branch or the digest.
    pub const fn golem(&self) -> &GolemStateView<HostStore> {
        &self.golem
    }

    pub const fn golem_mut(&mut self) -> &mut GolemStateView<HostStore> {
        &mut self.golem
    }

    fn accounts_dirty(&self) -> bool {
        !self.balances.is_empty() || !self.account_nonces.is_empty()
    }
}

// ── Arkiv's own state: straight through to GolemDB ──────────────────────────

impl<B, C, E> EntityStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn get_entity(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Option<Entity>, Self::Error> {
        Ok(self.golem.get_entity(address, read)?)
    }

    fn update_entity(&mut self, updates: EntityUpdates) -> Result<(), Self::Error> {
        Ok(self.golem.update_entity(updates)?)
    }

    fn get_uncommitted_deltas(&self) -> Result<Vec<EntityUpdates>, Self::Error> {
        Ok(self.golem.get_uncommitted_deltas()?)
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        let commitment = EntityStore::commit_store(&mut self.golem)?;
        self.commitment = Some(commitment);
        Ok(commitment)
    }
}

impl<B, C, E> EqualityIndexStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn get_equal_entities(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        Ok(self.golem.get_equal_entities(attribute, value, read)?)
    }

    fn get_prefixed_entities(
        &self,
        attribute: &[u8],
        prefix: &str,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        Ok(self.golem.get_prefixed_entities(attribute, prefix, read)?)
    }

    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), Self::Error> {
        Ok(EqualityIndexStore::apply_deltas(
            &mut self.golem,
            entity_updates,
        )?)
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        Ok(EqualityIndexStore::commit_store(&mut self.golem)?)
    }
}

impl<B, C, E> RangeIndexStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn get_within_range(
        &self,
        attribute: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error> {
        Ok(self.golem.get_within_range(attribute, low, high, read)?)
    }

    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), Self::Error> {
        Ok(RangeIndexStore::apply_deltas(
            &mut self.golem,
            entity_updates,
        )?)
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        Ok(RangeIndexStore::commit_store(&mut self.golem)?)
    }
}

/// Minting nonces were system-account storage slots on the MPT host; here they
/// are the account record's `mnt` cell, so they move with the entities.
impl<B, C, E> EntityCreationNoncesStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn get_entity_creation_nonce(
        &self,
        owner: UserAddress,
        read: ReadMode,
    ) -> Result<EntityCreationNonce, Self::Error> {
        Ok(self.golem.get_entity_creation_nonce(owner, read)?)
    }

    fn fetch_increment_entity_creation_nonce(
        &mut self,
        owner: UserAddress,
    ) -> Result<EntityCreationNonce, Self::Error> {
        Ok(self.golem.fetch_increment_entity_creation_nonce(owner)?)
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        Ok(EntityCreationNoncesStore::commit_store(&mut self.golem)?)
    }
}

impl<B, C, E> PruningStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn add_to_pruning_set(
        &mut self,
        entity: EntityAddress,
        pruning_meta: PruningMeta,
    ) -> Result<(), Self::Error> {
        Ok(self.golem.add_to_pruning_set(entity, pruning_meta)?)
    }

    fn peek_top(
        &self,
        top: u16,
        read: ReadMode,
    ) -> Result<Vec<(EntityAddress, PruningMeta)>, Self::Error> {
        Ok(self.golem.peek_top(top, read)?)
    }

    fn take_top(&mut self, top: u16) -> Result<Vec<(EntityAddress, PruningMeta)>, Self::Error> {
        Ok(self.golem.take_top(top)?)
    }

    fn commit_store(&mut self) -> Result<(), Self::Error> {
        Ok(PruningStore::commit_store(&mut self.golem)?)
    }
}

// ── The two lanes Ethereum also reads: staged into reth's accounts ──────────

impl<B, C, E> AccountBalancesStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

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
        self.base
            .borrow_mut()
            .get_balance(account.into())
            .map(|v| UserBalance::from_be_bytes(v.to_be_bytes()))
            .map_err(HostError::Backend)
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
        for (account, balance) in mem::take(&mut self.balances) {
            self.base
                .get_mut()
                .set_balance(
                    account.into(),
                    alloy_primitives::U256::from_be_bytes(balance.to_be_bytes()),
                )
                .map_err(HostError::Backend)?;
        }
        Ok(self.commitment.unwrap_or_default())
    }
}

impl<B, C, E> AccountNoncesStore for HostStateView<B, C>
where
    B: NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

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
        self.base
            .borrow_mut()
            .get_nonce(account.into())
            .map(UserNonce::new)
            .map_err(HostError::Backend)
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
        for (account, nonce) in mem::take(&mut self.account_nonces) {
            self.base
                .get_mut()
                .set_nonce(account.into(), nonce.get())
                .map_err(HostError::Backend)?;
        }
        Ok(self.commitment.unwrap_or_default())
    }
}

impl<B, C, E> StateView for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn base(&self) -> BlockRef {
        self.golem.base()
    }

    fn session(&self) -> SessionId {
        self.golem.session()
    }

    fn has_store(&self, _store: StoreKind) -> bool {
        true
    }

    fn graduate(self, block: BlockRef) -> Result<StateCommit, <Self as StateView>::Error> {
        if self.accounts_dirty() {
            return Err(HostError::Graduate("staged account writes remain"));
        }
        let commitment = self.commitment;
        let commit = self.golem.graduate(block)?;
        // The account lanes commit to the same digest the entities do: on this
        // host they are one branch, and reth's accounts are a mirror of it.
        let commitment = commitment.unwrap_or(commit.entities);
        Ok(StateCommit {
            balances: commitment,
            account_nonces: commitment,
            ..commit
        })
    }
}

/// Open a view over `branch` for one transaction, plus the revm overlay for
/// the two account lanes.
///
/// The branch belongs to the *block*, not the transaction: reth executes a
/// block's transactions in order and each must see the last one's writes, so
/// they share one branch and the frames inside it separate them.
pub fn host_manager<'a, DB: Database>(
    store: &HostStore,
    branch: BranchId,
    db: &'a mut DB,
    parent: BlockRef,
) -> Result<HostStateView<WriteOverlay<'a, DB>>, ViewError> {
    let origin = store.head();
    let golem = GolemStateView::new(store.clone(), branch, origin, parent, next_session());
    Ok(HostStateView::new(golem, WriteOverlay::new(db)))
}

/// Open the branch a block's execution runs on.
pub fn open_block_branch(store: &HostStore) -> Result<BranchId, ViewError> {
    Ok(store.begin(Some(store.head()))?)
}

/// Session ids for host-opened views: a process-local counter, which is all a
/// `SessionId` distinguishes.
fn next_session() -> SessionId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&std::process::id().to_be_bytes());
    bytes[8..].copy_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    SessionId(bytes)
}

impl<'a, DB: Database, C> HostStateView<WriteOverlay<'a, DB>, C> {
    /// End one transaction: hand back the account diff for reth, leaving the
    /// block's branch open for the next transaction.
    ///
    /// Nothing is committed here. A transaction is not a commit — the block
    /// is — and reth executes every block at least twice (once to build the
    /// payload, once to validate it). Committing mid-execution would make the
    /// second pass read state the first had already advanced, which is a
    /// receipt-root mismatch and a stalled chain.
    pub fn finish(self) -> Result<EvmState, ViewError> {
        Ok(self.into_base().into_state())
    }
}

/// The sealed candidates for each block height.
///
/// reth executes a block to build it and again to validate it, and may build
/// several candidates at one height. Each execution seals its branch — roots
/// computed, nothing written — and parks it here. When a block becomes
/// canonical its seal commits and the rest are discarded, which is exactly the
/// spec's "whichever is adopted commits and the rest leave no trace".
#[derive(Debug, Default)]
pub struct BlockSeals {
    pending: Mutex<BTreeMap<u64, Vec<(BranchId, SealedCommit)>>>,
}

impl BlockSeals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seal `branch` as a candidate for `height`.
    pub fn seal(
        &self,
        store: &HostStore,
        height: u64,
        branch: BranchId,
    ) -> Result<SealedCommit, ViewError> {
        let sealed = store.seal(branch)?;
        self.pending
            .lock()
            .expect("the seal registry's lock is never poisoned")
            .entry(height)
            .or_default()
            .push((branch, sealed));
        Ok(sealed)
    }

    /// A block at `height` became canonical: commit its candidate and drop
    /// every other candidate at or below that height.
    ///
    /// Which candidate? The first sealed at that height. One sequencer
    /// producing one payload per height seals the same roots twice — once
    /// building, once validating — so "first" is unambiguous there.
    ///
    /// ponytail: first-at-height. Competing payloads need the header to carry
    /// the Arkiv state root so the right seal can be named, which is what
    /// installing a `StateRootStrategy` buys.
    pub fn adopt(&self, store: &HostStore, height: u64) -> Result<Option<CommitId>, ViewError> {
        let stale: BTreeMap<u64, Vec<(BranchId, SealedCommit)>> = {
            let mut pending = self
                .pending
                .lock()
                .expect("the seal registry's lock is never poisoned");
            let future = pending.split_off(&(height + 1));
            core::mem::replace(&mut *pending, future)
        };

        let mut adopted = None;
        for (h, candidates) in stale {
            for (branch, _) in candidates {
                if adopted.is_none() && h == height {
                    adopted = Some(store.commit(branch)?);
                } else {
                    // A candidate that lost, or a height reth passed over.
                    // Discarding is exact: it was never state.
                    let _ = store.discard(branch);
                }
            }
        }
        Ok(adopted)
    }

    /// Drop a candidate that will never be adopted — a speculative call.
    pub fn abandon(store: &HostStore, branch: BranchId) {
        let _ = store.discard(branch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use arkiv_golemdb_state::GolemStateManager;
    use arkiv_interfaces::statemanager::StateManager;
    use arkiv_interfaces::store::Store;
    use arkiv_interfaces::store::reference::MemStore;
    use core::convert::Infallible;
    use std::collections::HashMap;

    /// The account half's backend: the two fields reth still owns.
    #[derive(Debug, Default)]
    struct MemAccounts {
        balances: HashMap<alloy_primitives::Address, U256>,
        nonces: HashMap<alloy_primitives::Address, u64>,
    }

    impl BalanceAccess for MemAccounts {
        type Error = Infallible;
        fn get_balance(&mut self, addr: alloy_primitives::Address) -> Result<U256, Infallible> {
            Ok(self.balances.get(&addr).copied().unwrap_or_default())
        }
        fn set_balance(
            &mut self,
            addr: alloy_primitives::Address,
            balance: U256,
        ) -> Result<(), Infallible> {
            self.balances.insert(addr, balance);
            Ok(())
        }
    }

    impl NonceAccess for MemAccounts {
        type Error = Infallible;
        fn get_nonce(&mut self, addr: alloy_primitives::Address) -> Result<u64, Infallible> {
            Ok(self.nonces.get(&addr).copied().unwrap_or_default())
        }
        fn set_nonce(
            &mut self,
            addr: alloy_primitives::Address,
            nonce: u64,
        ) -> Result<(), Infallible> {
            self.nonces.insert(addr, nonce);
            Ok(())
        }
    }

    const GENESIS: BlockRef = BlockRef {
        height: 0,
        hash: [0; 32],
    };

    // MemStore is a RefCell, so this handle is not Send + Sync. That is the
    // reference store's limitation; arkiv-valkey pins the real one as shareable.
    #[allow(clippy::arc_with_non_send_sync)]
    fn view() -> HostStateView<MemAccounts> {
        let store: HostStore = Arc::new(MemStore::new());
        let branch = store.begin(None).expect("begin");
        store
            .commit_tagged(branch, GENESIS.hash)
            .expect("tag genesis");
        let golem = GolemStateManager::new(store).view(GENESIS).expect("view");
        HostStateView::new(golem, MemAccounts::default())
    }

    /// The same suite `MptStateView` and `GolemStateView` pass. A host that
    /// splits state across two backends has to be indistinguishable from one
    /// that does not, or the split is observable and therefore consensus.
    #[test]
    fn conformance() {
        arkiv_interfaces::statemanager::conformance::run_all(&view);
    }

    /// The split itself: entities reach GolemDB, balances reach reth's accounts.
    #[test]
    fn each_lane_lands_in_its_own_backend() {
        let mut view = view();
        view.update_entity(EntityUpdates::create(Entity {
            key: [1; 32],
            ..Entity::default()
        }))
        .expect("create");
        view.set_balance([0xaa; 20], UserBalance::from_u64(500))
            .expect("balance");

        EntityStore::commit_store(&mut view).expect("entities");
        AccountBalancesStore::commit_store(&mut view).expect("balances");

        assert_eq!(
            view.base
                .borrow_mut()
                .get_balance([0xaa; 20].into())
                .unwrap(),
            U256::from(500),
            "the balance is in reth's account, where eth_getBalance reads",
        );
        assert_ne!(
            view.golem().digest().expect("digest"),
            [0u8; 32],
            "and the entity moved the store's digest",
        );
    }
}
