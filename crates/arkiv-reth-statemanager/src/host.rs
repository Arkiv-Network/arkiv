//! [`HostStateView`] — the reth host's `StateView`, on GolemDB.
//!
//! Every lane is GolemDB's: entities, the query index, the pruning set, the
//! entity-minting nonces, and — since the accounts cutover — balances and
//! transaction nonces too. All of it goes through [`GolemStateView`].
//!
//! # Why the `EvmState` diff is still here
//!
//! Account writes are *mirrored* into the [`EvmState`](crate::WriteOverlay)
//! diff reth's block executor commits. The diff is no longer where the values
//! live — reads never consult it — but reth executes a block's transactions
//! over a revm `State` that caches every account it loads. Without the diff,
//! the second transaction from a sender would see the first one's nonce and
//! balance unchanged, because nothing told reth's cache they moved.
//!
//! So the mirror is write-only, and it stays until reth stops running the
//! block loop over a cache of its own. The state root does not retire it.

use core::cell::RefCell;
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

/// One view over Arkiv's state for one block: GolemDB throughout, with the
/// two account lanes mirrored into reth's `EvmState` diff on the way past.
#[derive(Debug)]
pub struct HostStateView<B, C = PlaceholderCost> {
    golem: GolemStateView<HostStore>,
    // RefCell because the spec's reads are `&self` while the reth account
    // seams read with `&mut` (caches). Borrows never overlap: one per method.
    base: RefCell<B>,
    costs: C,
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

// ── The two lanes Ethereum also reads: GolemDB, mirrored into reth's diff ───

impl<B, C, E> HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    /// Copy the account's current balance into reth's diff.
    ///
    /// Read back from GolemDB rather than recomputed here: the store decides
    /// what a write means — `fetch_sub_balance` saturates, `create` leaves an
    /// untouched cell absent — and a second opinion about that is how the two
    /// sides drift apart.
    fn mirror_balance(&mut self, account: UserAddress) -> Result<(), HostError<E>> {
        let balance = self.golem.get_balance(account, ReadMode::ViewWithOverlay)?;
        self.base
            .get_mut()
            .set_balance(
                account.into(),
                alloy_primitives::U256::from_be_bytes(balance.to_be_bytes()),
            )
            .map_err(HostError::Backend)
    }

    /// Copy the account's current nonce into reth's diff.
    fn mirror_nonce(&mut self, account: UserAddress) -> Result<(), HostError<E>> {
        let nonce = self
            .golem
            .get_acc_nonce(account, ReadMode::ViewWithOverlay)?;
        self.base
            .get_mut()
            .set_nonce(account.into(), nonce.get())
            .map_err(HostError::Backend)
    }
}

impl<B, C, E> AccountBalancesStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn get_balance(
        &self,
        account: UserAddress,
        read: ReadMode,
    ) -> Result<UserBalance, Self::Error> {
        Ok(self.golem.get_balance(account, read)?)
    }

    fn fetch_add_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, Self::Error> {
        let before = self.golem.fetch_add_balance(account, amount)?;
        self.mirror_balance(account)?;
        Ok(before)
    }

    fn fetch_sub_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, Self::Error> {
        let before = self.golem.fetch_sub_balance(account, amount)?;
        self.mirror_balance(account)?;
        Ok(before)
    }

    fn compare_set_balance(
        &mut self,
        account: UserAddress,
        current: UserBalance,
        new: UserBalance,
    ) -> Result<bool, Self::Error> {
        if !self.golem.compare_set_balance(account, current, new)? {
            return Ok(false);
        }
        self.mirror_balance(account)?;
        Ok(true)
    }

    fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), Self::Error> {
        self.golem.set_balance(account, balance)?;
        self.mirror_balance(account)
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        Ok(AccountBalancesStore::commit_store(&mut self.golem)?)
    }
}

impl<B, C, E> AccountNoncesStore for HostStateView<B, C>
where
    B: BalanceAccess<Error = E> + NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = HostError<E>;

    fn get_acc_nonce(
        &self,
        account: UserAddress,
        read: ReadMode,
    ) -> Result<UserNonce, Self::Error> {
        Ok(self.golem.get_acc_nonce(account, read)?)
    }

    fn fetch_increment_acc_nonce(
        &mut self,
        account: UserAddress,
    ) -> Result<UserNonce, Self::Error> {
        let before = self.golem.fetch_increment_acc_nonce(account)?;
        self.mirror_nonce(account)?;
        Ok(before)
    }

    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        Ok(AccountNoncesStore::commit_store(&mut self.golem)?)
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
        Ok(self.golem.graduate(block)?)
    }
}

/// Open a view over `branch` for one transaction, plus the revm overlay for
/// the two account lanes.
///
/// The branch belongs to the *block*, not the transaction: reth executes a
/// block's transactions in order and each must see the last one's writes, so
/// they share one branch and the frames inside it separate them.
///
/// The base is always head, whatever block `parent` names. A simulation
/// against an older block — `eth_call` with a block number — runs against head
/// too: writes only ever extend the tip, and a branch over an older commit
/// could never be adopted. Reading the past is a commit-targeted read, which
/// takes no branch at all.
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

/// Which execution a seal belongs to.
///
/// Height alone does not identify one. reth builds a block and validates it,
/// and `BasicPayloadJob::resolve_kind` races a second build against an
/// unfinished one, so several executions at one height can be in flight at
/// once — on different threads. Their *contents* are what tell them apart.
///
/// A transaction is named by `(signer, nonce)`, not by hash: an `Evm` is handed
/// a `TxEnv`, which carries both of those but not the hash of the envelope they
/// came from. It is no weaker an identity, since a block cannot hold two
/// transactions with the same signer and nonce.
///
/// No parent hash. `BlockEnv` does not carry one, and it would add nothing:
/// a branch is always opened over head, and Arkiv never reorgs, so one height
/// has one parent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExecutionKey {
    /// The block being executed.
    pub height: u64,
    /// `(signer, nonce)` of each transaction executed, in order.
    pub transactions: Vec<(UserAddress, u64)>,
}

impl ExecutionKey {
    /// The key for a block at `height` that executed `transactions`.
    pub const fn new(height: u64, transactions: Vec<(UserAddress, u64)>) -> Self {
        Self {
            height,
            transactions,
        }
    }
}

/// The sealed candidates for each block height.
///
/// reth executes a block to build it and again to validate it, and may build
/// several candidates at one height. Each execution seals its branch — roots
/// computed, nothing written — and parks it here. When a block becomes
/// canonical its seal commits and the rest are discarded, which is exactly the
/// spec's "whichever is adopted commits and the rest leave no trace".
///
/// Seals are also indexed by [`ExecutionKey`], so the payload builder can ask
/// for the root of *its own* execution rather than the newest one at its
/// height. That index is what makes the built header's state root answerable
/// while two builds race.
#[derive(Debug, Default)]
pub struct BlockSeals {
    pending: Mutex<BTreeMap<u64, Vec<(BranchId, SealedCommit)>>>,
    by_execution: Mutex<BTreeMap<ExecutionKey, SealedCommit>>,
}

impl BlockSeals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seal `branch` as a candidate for the execution `key` describes.
    pub fn seal(
        &self,
        store: &HostStore,
        key: ExecutionKey,
        branch: BranchId,
    ) -> Result<SealedCommit, ViewError> {
        let sealed = store.seal(branch)?;
        self.pending
            .lock()
            .expect("the seal registry's lock is never poisoned")
            .entry(key.height)
            .or_default()
            .push((branch, sealed));
        // Last writer wins. Two executions sharing a key ran the same
        // transactions over the same height, so a deterministic store gives
        // them the same root and the overwrite is a no-op in everything that
        // matters. If it ever is not, the roots disagreed and the chain would
        // have halted anyway -- see `root_of`.
        self.by_execution
            .lock()
            .expect("the seal registry's lock is never poisoned")
            .insert(key, sealed);
        Ok(sealed)
    }

    /// The state root sealed for `key`, if that execution has sealed.
    ///
    /// `None` means no execution with these contents has sealed at this height.
    /// Callers treat that as "compute the root the usual way" rather than as a
    /// failure: a build that never reached `Evm::finish` leaves no seal, and
    /// `eth_call`-shaped executions never seal at all.
    pub fn root_of(&self, key: &ExecutionKey) -> Option<[u8; 32]> {
        self.by_execution
            .lock()
            .expect("the seal registry's lock is never poisoned")
            .get(key)
            .map(|sealed| sealed.state_root)
    }

    /// Forget the execution index at or below `height`, once a block there is
    /// canonical and no further build can target it.
    fn forget_executions_through(&self, height: u64) {
        let mut index = self
            .by_execution
            .lock()
            .expect("the seal registry's lock is never poisoned");
        index.retain(|key, _| key.height > height);
    }

    /// A block at `height` became canonical: commit its candidate and drop
    /// every other candidate at or below that height.
    ///
    /// Which candidate? The **last** sealed at that height.
    ///
    /// Not the first: a payload builder re-builds as transactions arrive, so a
    /// height accumulates several candidates and the early ones are the
    /// emptier ones. The last execution before a block goes canonical is reth
    /// validating the block it chose, so its seal is the one that matches the
    /// transactions actually in it.
    ///
    /// ponytail: last-at-height, which is ordering and not identity. Naming
    /// the right seal outright needs the header to carry the Arkiv state root,
    /// which is what installing a `StateRootStrategy` buys.
    pub fn adopt(
        &self,
        store: &HostStore,
        height: u64,
        block_hash: [u8; 32],
    ) -> Result<Option<CommitId>, ViewError> {
        let stale: BTreeMap<u64, Vec<(BranchId, SealedCommit)>> = {
            let mut pending = self
                .pending
                .lock()
                .expect("the seal registry's lock is never poisoned");
            let future = pending.split_off(&(height + 1));
            core::mem::replace(&mut *pending, future)
        };

        let mut winner = None;
        let mut losers = Vec::new();
        for (h, candidates) in stale {
            for (branch, sealed) in candidates {
                if h == height {
                    if let Some(previous) = winner.replace((branch, sealed)) {
                        losers.push(previous.0);
                    }
                } else {
                    // A height reth passed over.
                    losers.push(branch);
                }
            }
        }
        for branch in losers {
            // Discarding is exact: a branch that never committed was never
            // state.
            let _ = store.discard(branch);
        }
        self.forget_executions_through(height);
        match winner {
            // Tagged with the block hash, which is how a historical read finds
            // the commit for a past block.
            Some((branch, _)) => Ok(Some(store.commit_tagged(branch, block_hash)?)),
            None => Ok(None),
        }
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

    /// A balance lands in GolemDB, and the same value reaches reth's diff.
    ///
    /// Both halves matter. GolemDB is where the value lives and where every
    /// read comes from; the diff is what keeps reth's per-block account cache
    /// from serving the second transaction a stale sender.
    #[test]
    fn a_balance_lands_in_golemdb_and_is_mirrored_into_reths_diff() {
        let mut view = view();
        view.set_balance([0xaa; 20], UserBalance::from_u64(500))
            .expect("balance");

        assert_eq!(
            view.golem()
                .get_balance([0xaa; 20], ReadMode::ViewWithOverlay)
                .expect("golem read"),
            UserBalance::from_u64(500),
            "the balance is in the store, where every read now goes",
        );
        assert_eq!(
            view.base
                .borrow_mut()
                .get_balance([0xaa; 20].into())
                .unwrap(),
            U256::from(500),
            "and the same value is in reth's diff, for its own block cache",
        );
    }

    /// The mirror follows the store's arithmetic rather than repeating it.
    /// Debiting more than an account holds saturates at zero in GolemDB, and
    /// reth's diff has to say zero too — a mirror that recomputed the
    /// subtraction itself would be the place the two sides drift apart.
    #[test]
    fn an_oversized_debit_saturates_on_both_sides() {
        let mut view = view();
        view.set_balance([0xbb; 20], UserBalance::from_u64(10))
            .expect("fund");
        view.fetch_sub_balance([0xbb; 20], UserBalance::from_u64(999))
            .expect("debit");

        assert_eq!(
            view.get_balance([0xbb; 20], ReadMode::ViewWithOverlay)
                .expect("read"),
            UserBalance::from_u64(0),
        );
        assert_eq!(
            view.base
                .borrow_mut()
                .get_balance([0xbb; 20].into())
                .unwrap(),
            U256::ZERO,
        );
    }

    /// Entities still reach the store; the accounts cutover did not disturb them.
    #[test]
    fn an_entity_still_moves_the_stores_digest() {
        let mut view = view();
        view.update_entity(EntityUpdates::create(Entity {
            key: [1; 32],
            ..Entity::default()
        }))
        .expect("create");
        EntityStore::commit_store(&mut view).expect("entities");

        assert_ne!(view.golem().digest().expect("digest"), [0u8; 32]);
    }
}
