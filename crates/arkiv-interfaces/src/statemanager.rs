//! [`StateManager`] — the single seam over all of Arkiv's state. It only
//! speaks Arkiv types and never leaks host primitives.
//!
//! The manager mints [`StateView`]s at a block. Writes stage into the view's
//! overlay, reads pick [`ReadMode`], [`StateView::commit`] finalizes each
//! store, and a committed view graduates into a [`StateCommit`]. Two views at
//! the same block are independent — that is simulation. There is no rewind.

use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt::Debug;
use core::ops::Bound;

use crate::entity::{Attribute, AttributeValue, CreationFlags, Entity};
use crate::execution::Op;
use crate::primitives::{
    BlockNumber, EntityAddress, EntityCreationNonce, Gas, Hash, UserAddress, UserBalance, UserNonce,
};
use crate::query::QueryStats;

/// A 32-byte commitment over a store's contents.
pub type Commitment = [u8; 32];

/// One committed block. Its parent is `(height - 1, <parent's hash>)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BlockRef {
    pub height: BlockNumber,
    pub hash: Hash,
}

impl BlockRef {
    pub const fn new(height: BlockNumber, hash: Hash) -> Self {
        Self { height, hash }
    }
}

/// The identity of one open [`StateView`] — auto-generated at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub [u8; 16]);

/// Which state a read answers from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadMode {
    /// The base as of the view's block, unaffected by this view's writes.
    ViewOnBase,
    /// The base plus this view's staged (uncommitted) writes.
    #[default]
    ViewWithOverlay,
}

/// The stores a [`StateView`] can present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StoreKind {
    Entities,
    EqualityIndex,
    RangeIndex,
    AccountBalances,
    AccountNonces,
    EntityCreationNonces,
    Pruning,
}

/// A partial write to one entity: `None` leaves a field alone, `attributes`
/// replaces the whole set, `delete` is a tombstone. Creation is an update to
/// an absent entity with every field set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntityUpdates {
    pub entity: EntityAddress,
    pub delete: bool,
    pub creator: Option<UserAddress>,
    pub owner: Option<UserAddress>,
    pub created_at_block: Option<BlockNumber>,
    pub last_modified_at_block: Option<BlockNumber>,
    pub expires_at: Option<BlockNumber>,
    pub creation_flags: Option<CreationFlags>,
    pub content_type: Option<Vec<u8>>,
    pub payload: Option<Vec<u8>>,
    pub attributes: Option<Vec<Attribute>>,
}

impl EntityUpdates {
    pub fn create(entity: Entity) -> Self {
        Self {
            entity: entity.key,
            delete: false,
            creator: Some(entity.creator),
            owner: Some(entity.owner),
            created_at_block: Some(entity.created_at_block),
            last_modified_at_block: Some(entity.last_modified_at_block),
            expires_at: Some(entity.expires_at),
            creation_flags: Some(entity.creation_flags),
            content_type: Some(entity.content_type),
            payload: Some(entity.payload),
            attributes: Some(entity.attributes),
        }
    }

    pub fn deletion(entity: EntityAddress) -> Self {
        Self {
            entity,
            delete: true,
            ..Self::default()
        }
    }

    pub fn apply_to(&self, entity: &mut Entity) {
        entity.key = self.entity;
        if let Some(v) = self.creator {
            entity.creator = v;
        }
        if let Some(v) = self.owner {
            entity.owner = v;
        }
        if let Some(v) = self.created_at_block {
            entity.created_at_block = v;
        }
        if let Some(v) = self.last_modified_at_block {
            entity.last_modified_at_block = v;
        }
        if let Some(v) = self.expires_at {
            entity.expires_at = v;
        }
        if let Some(v) = self.creation_flags {
            entity.creation_flags = v;
        }
        if let Some(v) = &self.content_type {
            entity.content_type = v.clone();
        }
        if let Some(v) = &self.payload {
            entity.payload = v.clone();
        }
        if let Some(v) = &self.attributes {
            entity.attributes = v.clone();
        }
    }
}

/// How urgently a tombstoned entity should be pruned. Higher prunes first.
pub type PruningPriority = u8;

/// Orders the pruning set: "less" is "pruned sooner" — higher priority first,
/// then older introduction. The order is consensus; the store breaks ties by
/// ascending entity address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PruningMeta {
    pub priority: PruningPriority,
    pub introduced_at: BlockNumber,
}

impl Ord for PruningMeta {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| self.introduced_at.cmp(&other.introduced_at))
    }
}

impl PartialOrd for PruningMeta {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The account balances. Arithmetic is saturating; policy belongs to the
/// business layer.
pub trait AccountBalancesStore {
    type Error: Debug;

    fn get_balance(&self, account: UserAddress, read: ReadMode)
    -> Result<UserBalance, Self::Error>;

    /// Returns the balance **before** the addition.
    fn fetch_add_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, Self::Error>;

    /// Returns the balance **before** the subtraction.
    fn fetch_sub_balance(
        &mut self,
        account: UserAddress,
        amount: UserBalance,
    ) -> Result<UserBalance, Self::Error>;

    /// Set to `new` only if the balance is `current`.
    fn compare_set_balance(
        &mut self,
        account: UserAddress,
        current: UserBalance,
        new: UserBalance,
    ) -> Result<bool, Self::Error>;

    fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), Self::Error>;

    fn commit_store(&mut self) -> Result<Commitment, Self::Error>;
}

/// The account transaction nonces — replay protection.
pub trait AccountNoncesStore {
    type Error: Debug;

    fn get_acc_nonce(&self, account: UserAddress, read: ReadMode)
    -> Result<UserNonce, Self::Error>;

    /// Returns the nonce **before** the increment.
    fn fetch_increment_acc_nonce(&mut self, account: UserAddress)
    -> Result<UserNonce, Self::Error>;

    fn commit_store(&mut self) -> Result<Commitment, Self::Error>;
}

/// The entity-creation nonces: an input to every minted [`EntityAddress`].
pub trait EntityCreationNoncesStore {
    type Error: Debug;

    fn get_entity_creation_nonce(
        &self,
        owner: UserAddress,
        read: ReadMode,
    ) -> Result<EntityCreationNonce, Self::Error>;

    /// Returns the nonce **before** the increment.
    fn fetch_increment_entity_creation_nonce(
        &mut self,
        owner: UserAddress,
    ) -> Result<EntityCreationNonce, Self::Error>;

    fn commit_store(&mut self) -> Result<Commitment, Self::Error>;
}

/// The entities: [`EntityAddress`] → [`Entity`].
pub trait EntityStore {
    type Error: Debug;

    fn get_entity(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Option<Entity>, Self::Error>;

    fn update_entity(&mut self, updates: EntityUpdates) -> Result<(), Self::Error>;

    /// One net-against-base entry per touched entity, ascending — what the
    /// index stores fold in.
    fn get_uncommitted_deltas(&self) -> Result<Vec<EntityUpdates>, Self::Error>;

    fn commit_store(&mut self) -> Result<Commitment, Self::Error>;
}

/// The equality index: `(attribute, typed value) → entities`. The type is part
/// of the bucket. String attributes also answer `STARTSWITH` here.
pub trait EqualityIndexStore {
    type Error: Debug;

    /// Ascending entity order.
    fn get_equal_entities(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error>;

    /// Raw byte prefix, no normalization. Ascending entity order.
    fn get_prefixed_entities(
        &self,
        attribute: &[u8],
        prefix: &str,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error>;

    /// Removals are derived by diffing against the base, so each update must be
    /// net-against-base ([`EntityStore::get_uncommitted_deltas`]'s shape); a
    /// repeated entity replaces its earlier delta.
    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), Self::Error>;

    fn commit_store(&mut self) -> Result<(), Self::Error>;
}

/// The range index: ordered lookups over the range-indexable types.
pub trait RangeIndexStore {
    type Error: Debug;

    /// The bounds' type names the scanned bucket set: mixed-type bounds match
    /// nothing, and a host may refuse a fully unbounded pair. Ascending entity
    /// order.
    fn get_within_range(
        &self,
        attribute: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, Self::Error>;

    /// Same contract as [`EqualityIndexStore::apply_deltas`].
    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), Self::Error>;

    fn commit_store(&mut self) -> Result<(), Self::Error>;
}

/// The pruning set, ordered by [`PruningMeta`]. Node-local and uncommitted,
/// but the order is consensus.
pub trait PruningStore {
    type Error: Debug;

    fn add_to_pruning_set(
        &mut self,
        entity: EntityAddress,
        pruning_meta: PruningMeta,
    ) -> Result<(), Self::Error>;

    fn peek_top(
        &self,
        top: u16,
        read: ReadMode,
    ) -> Result<Vec<(EntityAddress, PruningMeta)>, Self::Error>;

    fn take_top(&mut self, top: u16) -> Result<Vec<(EntityAddress, PruningMeta)>, Self::Error>;

    fn commit_store(&mut self) -> Result<(), Self::Error>;
}

/// What graduating a committed [`StateView`] produces. The index and pruning
/// stores finalize without committing to a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateCommit {
    pub block: BlockRef,
    pub parent: BlockRef,
    pub entities: Commitment,
    pub balances: Commitment,
    pub account_nonces: Commitment,
    pub entity_creation_nonces: Commitment,
}

/// One view over the state as committed through one block: every store behind
/// one handle with one error type, plus an overlay for what this view stages.
pub trait StateView:
    EntityStore<Error = <Self as StateView>::Error>
    + EqualityIndexStore<Error = <Self as StateView>::Error>
    + RangeIndexStore<Error = <Self as StateView>::Error>
    + AccountBalancesStore<Error = <Self as StateView>::Error>
    + AccountNoncesStore<Error = <Self as StateView>::Error>
    + EntityCreationNoncesStore<Error = <Self as StateView>::Error>
    + PruningStore<Error = <Self as StateView>::Error>
{
    type Error: Debug;

    /// The block this view reads as its base.
    fn base(&self) -> BlockRef;

    fn session(&self) -> SessionId;

    /// The backend retains a bounded window of heights per store, so an old
    /// view may lack some.
    fn has_store(&self, store: StoreKind) -> bool;

    // Each store, narrowed to its own trait — `_mut` for the write half. The
    // view *is* every store; these just present one at a time.

    fn entities(&self) -> &impl EntityStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn entities_mut(&mut self) -> &mut impl EntityStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn equality_index(&self) -> &impl EqualityIndexStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn equality_index_mut(
        &mut self,
    ) -> &mut impl EqualityIndexStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn range_index(&self) -> &impl RangeIndexStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn range_index_mut(&mut self) -> &mut impl RangeIndexStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn account_balances(&self) -> &impl AccountBalancesStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn account_balances_mut(
        &mut self,
    ) -> &mut impl AccountBalancesStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn account_nonces(&self) -> &impl AccountNoncesStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn account_nonces_mut(
        &mut self,
    ) -> &mut impl AccountNoncesStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn entity_creation_nonces(
        &self,
    ) -> &impl EntityCreationNoncesStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn entity_creation_nonces_mut(
        &mut self,
    ) -> &mut impl EntityCreationNoncesStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn pruning(&self) -> &impl PruningStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    fn pruning_mut(&mut self) -> &mut impl PruningStore<Error = <Self as StateView>::Error>
    where
        Self: Sized,
    {
        self
    }

    /// Commit every store, entities before the indexes.
    fn commit(&mut self) -> Result<(), <Self as StateView>::Error> {
        EntityStore::commit_store(self)?;
        EqualityIndexStore::commit_store(self)?;
        RangeIndexStore::commit_store(self)?;
        AccountBalancesStore::commit_store(self)?;
        AccountNoncesStore::commit_store(self)?;
        EntityCreationNoncesStore::commit_store(self)?;
        PruningStore::commit_store(self)?;
        Ok(())
    }

    /// Errors if anything staged remains uncommitted or `block` doesn't extend
    /// [`base`](Self::base).
    fn graduate(self, block: BlockRef) -> Result<StateCommit, <Self as StateView>::Error>
    where
        Self: Sized;
}

#[cfg(feature = "conformance")]
pub mod conformance;

/// Mints [`StateView`]s, and prices work. Pricing is consensus: it decides
/// out-of-gas, so two managers at the same state must answer identically.
pub trait StateManager {
    type Error: Debug;
    type View: StateView<Error = Self::Error>;

    /// Open a view over the state as committed through `at`, with a fresh
    /// [`SessionId`].
    fn view(&self, at: BlockRef) -> Result<Self::View, Self::Error>;

    fn get_operation_cost(&mut self, op: &Op) -> Result<Gas, Self::Error>;

    fn get_query_cost(&mut self, stats: &QueryStats) -> Result<Gas, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn pruning_meta_orders_urgent_first() {
        let urgent = PruningMeta {
            priority: 10,
            introduced_at: 100,
        };
        let lazy = PruningMeta {
            priority: 1,
            introduced_at: 5,
        };
        assert!(urgent < lazy, "higher priority wins over earlier arrival");

        let old = PruningMeta {
            priority: 5,
            introduced_at: 10,
        };
        let new = PruningMeta {
            priority: 5,
            introduced_at: 11,
        };
        assert!(old < new, "same priority: first in, first pruned");
    }

    #[test]
    fn entity_updates_apply_only_what_is_set() {
        let mut entity = Entity {
            key: [1; 32],
            owner: [2; 20],
            expires_at: 50,
            payload: vec![1, 2, 3],
            ..Entity::default()
        };
        let updates = EntityUpdates {
            entity: [1; 32],
            owner: Some([9; 20]),
            expires_at: Some(80),
            ..EntityUpdates::default()
        };
        updates.apply_to(&mut entity);
        assert_eq!(entity.owner, [9; 20]);
        assert_eq!(entity.expires_at, 80);
        assert_eq!(entity.payload, vec![1, 2, 3], "untouched field survives");
    }

    #[test]
    fn create_round_trips_the_whole_entity() {
        let original = Entity {
            key: [7; 32],
            creator: [3; 20],
            owner: [4; 20],
            created_at_block: 10,
            last_modified_at_block: 10,
            expires_at: 99,
            payload: vec![0xAA],
            ..Entity::default()
        };
        let mut rebuilt = Entity::default();
        EntityUpdates::create(original.clone()).apply_to(&mut rebuilt);
        assert_eq!(rebuilt, original);
    }
}
