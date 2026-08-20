//! [`StateManager`] — the single seam over **all** of Arkiv's state.
//!
//! The two stores in [`state`](crate::state) hold the entities and their query
//! index, but executing a transaction touches more than those: the caller's
//! balance is charged, its transaction nonce advances, its entity-minting nonce
//! feeds key derivation, and tombstoned entities queue up for the per-block
//! purge. Each lane is small — yet a host that hands them out separately leaks
//! its layout (who stores what where, in how many trees) into every consumer.
//!
//! [`StateManager`] is the one handle the business layer, the executor, and the
//! read paths go through instead. What it conceals is exactly what varies by
//! host: the reth host multiplexes everything onto Ethereum's single keccak-MPT
//! account state (entities as account code, index bitmaps and B-trees as code
//! and storage slots, 32-byte Arkiv keys truncated to 20-byte account
//! addresses); another host may keep every store in its own tree. Swapping that
//! engine must never move this API.
//!
//! ## The stores behind it
//!
//! **Committed** — consensus state, covered by commitments:
//!
//! - account balances — [`AccountBalancesStore`]
//! - account transaction nonces — [`AccountNoncesStore`]
//! - entity-minting nonces — [`EntityCreationNoncesStore`]
//! - the entities — [`EntityStore`]
//! - the query index — [`AuxiliaryStore`]
//!
//! **Uncommitted** — node-local bookkeeping with no commitment, always
//! rederivable from the committed stores:
//!
//! - the pruning map — [`PruningMap`]
//!
//! Each lane is its own trait so partial hosts (a read-only snapshot, a test
//! harness) can implement just what they serve; [`StateManager`] requires all of
//! them **with one shared error type**, so a consumer holds one bound and one
//! failure path.
//!
//! ## Shallow copies
//!
//! [`shallow_copy`](StateManager::shallow_copy) is the load-bearing capability:
//! a cheap, independent copy sharing the committed base. The caller may be about
//! to simulate, or about to actually build a block — the manager doesn't know
//! and doesn't need to. Work runs against the copy either way; dropping it is
//! the simulation outcome, keeping it (and letting the host persist its staged
//! changes) is the block-building one. Writes to a copy must never be visible
//! through the original or through sibling copies.

use alloc::vec::Vec;

use crate::execution::{BlockDraft, Op};
use crate::primitives::{Address, Balance, BlockNumber, EntityKey, EntityNonce, Gas};
use crate::query::QueryStats;
use crate::state::{AuxiliaryStore, EntityStore};

/// Holds the **account balances**.
///
/// Reads and writes are raw: what a debit means — checked against funds, or
/// saturating — is the business layer's policy, expressed with
/// [`Balance`]'s checked/saturating arithmetic, not the store's.
pub trait AccountBalancesStore {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's balance; [`Balance::ZERO`] if the account doesn't exist.
    fn get_balance(&mut self, account: Address) -> Result<Balance, Self::Error>;

    /// Set the account's balance, creating the account if needed.
    fn set_balance(&mut self, account: Address, balance: Balance) -> Result<(), Self::Error>;
}

/// Holds the **account transaction nonces** — replay protection, advanced once
/// per transaction.
///
/// This is the *other* nonce, deliberately a bare `u64`: the entity-minting
/// nonce is the newtyped [`EntityNonce`] in [`EntityCreationNoncesStore`], and
/// the two advance at different rates (see [`EntityNonce`]'s docs).
pub trait AccountNoncesStore {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's transaction nonce; `0` if the account doesn't exist.
    fn get_account_nonce(&mut self, account: Address) -> Result<u64, Self::Error>;

    /// Set the account's transaction nonce.
    fn set_account_nonce(&mut self, account: Address, nonce: u64) -> Result<(), Self::Error>;
}

/// Holds the **entity-minting nonces**: how many entities each account has
/// created, an input to every minted [`EntityKey`].
pub trait EntityCreationNoncesStore {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The owner's minting nonce; [`EntityNonce::ZERO`] if it never created one.
    fn get_entity_nonce(&mut self, owner: Address) -> Result<EntityNonce, Self::Error>;

    /// Advance the owner's minting nonce by `by` (one per entity created),
    /// returning the value it had **before** the advance — the nonce the batch's
    /// first create was derived with.
    fn advance_entity_nonce(&mut self, owner: Address, by: u64)
    -> Result<EntityNonce, Self::Error>;
}

/// How urgently a tombstoned entity should be pruned. Higher prunes first.
///
/// The purge works a bounded budget per block; priority decides who gets that
/// budget when more is due than fits.
pub type PruningPriority = u8;

/// The **pruning map**: which entities are tombstoned, the block at which each
/// is due for physical removal by the per-block purge, and how urgently.
///
/// Node-local and uncommitted — it carries no commitment and never feeds one.
/// Its contents must be rederivable from the committed stores (every entity's
/// `expires_at` is committed state), so a restarted node rebuilds it rather than
/// trusting a sidecar. But *because* the purge turns its answers into block
/// deltas, [`pruning_due`](PruningMap::pruning_due) must be deterministic:
/// highest [`PruningPriority`] first, ties in ascending entity-key order, so
/// two nodes purge identically.
pub trait PruningMap {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// Record (or move, if already recorded) `entity`'s removal to `prune_at`,
    /// with `priority` deciding its place in the queue once due.
    fn schedule_pruning(
        &mut self,
        entity: EntityKey,
        prune_at: BlockNumber,
        priority: PruningPriority,
    ) -> Result<(), Self::Error>;

    /// Every entity due at `block` (scheduled at or before it): highest
    /// priority first, ties in ascending key order.
    fn pruning_due(&mut self, block: BlockNumber) -> Result<Vec<EntityKey>, Self::Error>;

    /// Forget `entities` — they were physically removed (or deleted early).
    fn clear_pruning(&mut self, entities: &[EntityKey]) -> Result<(), Self::Error>;
}

/// The umbrella: every state lane behind **one handle with one error type**,
/// plus the lifecycle a blockchain backend owes its host.
///
/// The supertrait bounds pin each lane's `Error` to
/// [`StateManager::Error`], so generic consumers write one bound
/// (`M: StateManager`) and one `?`. Note the two `apply_delta` /
/// `commitment` pairs (entities and index) share method names — call them
/// through the trait ([`EntityStore::apply_delta`], …) or via
/// [`apply_draft`](StateManager::apply_draft), which applies both lanes.
///
/// **Pricing is asked of the manager directly**
/// ([`get_operation_cost`](StateManager::get_operation_cost)): "how much would
/// you charge for operation `o`?" The manager answers because pricing belongs
/// with state — a schedule may consult any committed store (storage pressure,
/// an entity's current size, …) — and the answer is **consensus**: it decides
/// out-of-gas, so it must be deterministic, identical on every node at the
/// same state.
pub trait StateManager:
    EntityStore<Error = <Self as StateManager>::Error>
    + AuxiliaryStore<Error = <Self as StateManager>::Error>
    + AccountBalancesStore<Error = <Self as StateManager>::Error>
    + AccountNoncesStore<Error = <Self as StateManager>::Error>
    + EntityCreationNoncesStore<Error = <Self as StateManager>::Error>
    + PruningMap<Error = <Self as StateManager>::Error>
{
    /// The one error type every lane of this manager fails with.
    type Error: core::fmt::Debug;

    /// What this manager would charge for `op`, against current committed
    /// state.
    ///
    /// Deterministic and consensus-critical: two managers at the same state
    /// must answer identically. `&mut self` because the answer may read any
    /// committed store (this crate's reads are `&mut` throughout).
    fn get_operation_cost(&mut self, op: &Op) -> Result<Gas, <Self as StateManager>::Error>;

    /// What this manager would charge for a query that did `stats` worth of
    /// work. Same determinism contract as
    /// [`get_operation_cost`](StateManager::get_operation_cost).
    fn get_query_cost(&mut self, stats: &QueryStats) -> Result<Gas, <Self as StateManager>::Error>;

    /// A cheap, independent copy sharing the committed base.
    ///
    /// The copy sees everything committed plus this manager's staged-but-
    /// uncommitted changes as they are *now*; from here on, writes to either
    /// side are invisible to the other. Drop the copy to have simulated;
    /// keep it to have built.
    fn shallow_copy(&self) -> Self
    where
        Self: Sized;

    /// Apply one block's staged changes — both lanes of `draft`, entities
    /// before index (the index delta may allocate ids for keys the entity
    /// delta just wrote).
    fn apply_draft(&mut self, draft: &BlockDraft) -> Result<(), <Self as StateManager>::Error> {
        EntityStore::apply_delta(self, &draft.entities)?;
        AuxiliaryStore::apply_delta(self, &draft.auxiliary)
    }

    /// Rewind every store to its state as of `block` — a reorg, the way any
    /// blockchain backend must be able to step back.
    ///
    /// A host whose storage engine reorgs *itself* (reth rewinds its own MPT)
    /// may reject this and instead hand out a fresh manager over the target
    /// block's state; a host that owns its storage implements it for real.
    fn rewind_to(&mut self, block: BlockNumber) -> Result<(), <Self as StateManager>::Error>;
}
