//! [`StateManager`] — the single seam over all of Arkiv's state. It only
//! speaks Arkiv types and never leaks host primitives.
//!
//! The manager mints [`StateView`]s at a block. Writes stage into the view's
//! overlay, reads pick [`ReadMode`], [`StateView::commit`] finalizes each
//! store, and a committed view graduates into a [`StateCommit`]. Two views at
//! the same block are independent — that is simulation. There is no rewind.

use alloc::vec::Vec;
use core::fmt::Debug;
use core::ops::Bound;

use crate::entity::{Attribute, AttributeValue, CreationFlags, Entity};
use crate::primitives::{
    BlockNumber, EntityAddress, EntityCreationNonce, Hash, UserAddress, UserBalance, UserNonce,
};
use crate::store::{Metered, RecordMeta};

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
    pub created_at: Option<BlockNumber>,
    pub updated_at: Option<BlockNumber>,
    pub expires_at: Option<BlockNumber>,
    pub creation_flags: Option<CreationFlags>,
    pub attributes: Option<Vec<Attribute>>,
}

impl EntityUpdates {
    pub fn create(entity: Entity) -> Self {
        Self {
            entity: entity.key,
            delete: false,
            creator: Some(entity.creator),
            owner: Some(entity.owner),
            created_at: Some(entity.created_at),
            updated_at: Some(entity.updated_at),
            expires_at: Some(entity.expires_at),
            creation_flags: Some(entity.creation_flags),
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
        if let Some(v) = self.created_at {
            entity.created_at = v;
        }
        if let Some(v) = self.updated_at {
            entity.updated_at = v;
        }
        if let Some(v) = self.expires_at {
            entity.expires_at = v;
        }
        if let Some(v) = self.creation_flags {
            entity.creation_flags = v;
        }
        if let Some(v) = &self.attributes {
            entity.attributes = v.clone();
        }
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
///
/// Reads and writes return what they cost: minting-nonce traffic is entity
/// cost. A caller that does not charge for it says so with
/// [`Metered::into_value`].
pub trait EntityCreationNoncesStore {
    type Error: Debug;

    fn get_entity_creation_nonce(
        &self,
        owner: UserAddress,
        read: ReadMode,
    ) -> Result<Metered<EntityCreationNonce>, Self::Error>;

    /// Returns the nonce **before** the increment, and the cost of the read
    /// and the write together.
    fn fetch_increment_entity_creation_nonce(
        &mut self,
        owner: UserAddress,
    ) -> Result<Metered<EntityCreationNonce>, Self::Error>;

    fn commit_store(&mut self) -> Result<Commitment, Self::Error>;
}

/// An entity as the store holds it, and the size its record takes there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntity {
    pub entity: Entity,
    /// Not optional: an entity read fetches the whole record, so a missing
    /// size is a store fault, never an unknown one.
    pub meta: RecordMeta,
}

/// The entities: [`EntityAddress`] → [`Entity`].
///
/// Reads and writes return what they cost, a read that finds nothing
/// included. A caller that does not charge for it says so with
/// [`Metered::into_value`].
pub trait EntityStore {
    type Error: Debug;

    /// The entity with its record's size, in one store call: what pricing
    /// needs as the size before a write.
    fn get_entity_with_meta(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Metered<Option<StoredEntity>>, Self::Error>;

    /// The entity's system fields with its record's size, without its
    /// attributes: what an op needs that leaves them alone. The entity comes
    /// back with no attributes. By default the full read, attributes dropped;
    /// a store that reads by the byte reads only the system cells.
    fn get_entity_system_fields(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Metered<Option<StoredEntity>>, Self::Error> {
        Ok(self.get_entity_with_meta(address, read)?.map(|found| {
            found.map(|mut stored| {
                stored.entity.attributes.clear();
                stored
            })
        }))
    }

    /// The same read, without the size. A caller that needs the size uses
    /// [`get_entity_with_meta`](Self::get_entity_with_meta) rather than a
    /// second read.
    fn get_entity(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Metered<Option<Entity>>, Self::Error> {
        Ok(self
            .get_entity_with_meta(address, read)?
            .map(|found| found.map(|stored| stored.entity)))
    }

    /// The receipt covers every store call the update made, its own read
    /// included. Errors do not roll back writes: that is what a frame is for.
    fn update_entity(&mut self, updates: EntityUpdates) -> Result<Metered<()>, Self::Error>;

    /// One net-against-base entry per touched entity, ascending.
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
}

/// What graduating a committed [`StateView`] produces.
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
{
    type Error: Debug;

    /// The block this view reads as its base.
    fn base(&self) -> BlockRef;

    fn session(&self) -> SessionId;

    /// The backend retains a bounded window of heights per store, so an old
    /// view may lack some.
    fn has_store(&self, store: StoreKind) -> bool;

    /// Open the transaction's frame: a safe point that
    /// [`discard_frame`](Self::discard_frame) rolls entity and minting-nonce
    /// writes back to, delta log included.
    ///
    /// One frame per transaction, and frames do not nest: a second
    /// `begin_frame` while one is open is an error. Account writes belong
    /// outside the frame. Costs are not rolled back: receipts are return
    /// values, so what a discarded frame spent stays with the caller.
    fn begin_frame(&mut self) -> Result<(), <Self as StateView>::Error>;

    /// Keep the open frame's writes.
    fn merge_frame(&mut self) -> Result<(), <Self as StateView>::Error>;

    /// Roll the open frame's writes back.
    fn discard_frame(&mut self) -> Result<(), <Self as StateView>::Error>;

    // Each store, narrowed to its own trait — `_mut` for the write half. The
    // view *is* every store; these just present one at a time. The indexes are
    // read-only: the store maintains them as entities are written.

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

    fn range_index(&self) -> &impl RangeIndexStore<Error = <Self as StateView>::Error>
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

    /// Commit every store.
    fn commit(&mut self) -> Result<(), <Self as StateView>::Error> {
        EntityStore::commit_store(self)?;
        AccountBalancesStore::commit_store(self)?;
        AccountNoncesStore::commit_store(self)?;
        EntityCreationNoncesStore::commit_store(self)?;
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

/// Mints [`StateView`]s.
pub trait StateManager {
    type Error: Debug;
    type View: StateView<Error = Self::Error>;

    /// Open a view over the state as committed through `at`, with a fresh
    /// [`SessionId`].
    fn view(&self, at: BlockRef) -> Result<Self::View, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn entity_updates_apply_only_what_is_set() {
        let mut entity = Entity {
            key: [1; 32],
            owner: [2; 20],
            expires_at: 50,
            created_at: 7,
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
        assert_eq!(entity.created_at, 7, "untouched field survives");
    }

    #[test]
    fn create_round_trips_the_whole_entity() {
        let original = Entity {
            key: [7; 32],
            creator: [3; 20],
            owner: [4; 20],
            created_at: 10,
            updated_at: 10,
            expires_at: 99,
            attributes: vec![Attribute::unindexed(
                b"body".to_vec(),
                AttributeValue::Bytes(vec![0xAA]),
            )],
            ..Entity::default()
        };
        let mut rebuilt = Entity::default();
        EntityUpdates::create(original.clone()).apply_to(&mut rebuilt);
        assert_eq!(rebuilt, original);
    }
}
