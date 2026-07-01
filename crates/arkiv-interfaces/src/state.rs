//! The two state stores and the per-block deltas that change them.

use alloc::vec::Vec;

use crate::primitives::{BlockNumber, EntityKey, Hash};
use crate::query::{PageParams, Query, QueryMatches};

/// Holds the **entities** — the state that is Arkiv's reason to exist.
///
/// It is just a map from [`EntityKey`] to bytes, plus a
/// [`commitment`](EntityStore::commitment) over the whole map. You read one entity
/// with [`get`](EntityStore::get); you apply a whole block's changes at once with
/// [`apply_delta`](EntityStore::apply_delta). The bytes are opaque — encoding an
/// [`Entity`](crate::entity::Entity) to and from them is the executor's and query
/// layer's job, not this store's.
///
/// The host decides where the bytes actually live and handles any low-level
/// bookkeeping underneath (persistence markers, tombstones).
///
/// Every method takes `&mut self`, reads included: a host's read path may need to
/// update a cache.
pub trait EntityStore {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// This entity's bytes, or `None` if there is no such entity.
    fn get(&mut self, entity: EntityKey) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Apply one block's entity changes: the writes and removals in `delta`.
    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Self::Error>;

    /// Commitment over every entity. Separate from the index's
    /// ([`AuxiliaryStore::commitment`]).
    fn commitment(&mut self) -> Result<Hash, Self::Error>;
}

/// Optional: read entities as of a **past block**, for a host that keeps — or can
/// reconstruct — history. Extends [`EntityStore`] (whose reads are at the tip).
pub trait HistoricalEntityStore: EntityStore {
    /// This entity's bytes as of block `at`, or `None` if it didn't exist then.
    fn get_at(
        &mut self,
        entity: EntityKey,
        at: BlockNumber,
    ) -> Result<Option<Vec<u8>>, Self::Error>;
}

/// Holds the **query index** over the entities.
///
/// Where the [`EntityStore`] is a dumb byte map, this store understands the query
/// language: give it a [`Query`] and it returns the keys of the entities that
/// match. It is part of consensus too, kept up to date as blocks apply, and
/// commits to its own contents separately from the entities.
pub trait AuxiliaryStore {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The keys matching `query`, one page at a time, with the statistics of the
    /// work done.
    fn evaluate(&mut self, query: &Query, page: PageParams) -> Result<QueryMatches, Self::Error>;

    /// Apply one block's index changes.
    fn apply_delta(&mut self, delta: &BlockAuxiliaryStoreDelta) -> Result<(), Self::Error>;

    /// Commitment over the index. Separate from the entities'
    /// ([`EntityStore::commitment`]).
    fn commitment(&mut self) -> Result<Hash, Self::Error>;
}

/// Optional: evaluate queries against the index as of a **past block**. Extends
/// [`AuxiliaryStore`] (whose evaluation is at the tip).
pub trait HistoricalAuxiliaryStore: AuxiliaryStore {
    /// The keys matching `query` as of block `at`, one page at a time, with the
    /// statistics of the work done.
    fn evaluate_at(
        &mut self,
        query: &Query,
        page: PageParams,
        at: BlockNumber,
    ) -> Result<QueryMatches, Self::Error>;
}

/// One block's changes to the [`EntityStore`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockEntityStoreDelta {
    /// Entities written this block (key → new bytes).
    pub puts: Vec<(EntityKey, Vec<u8>)>,
    /// Entities removed this block.
    pub deletes: Vec<EntityKey>,
}

/// One block's changes to the [`AuxiliaryStore`], with one entry per entity
/// touched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockAuxiliaryStoreDelta {
    /// The per-entity index changes for this block.
    pub entities: Vec<AuxiliaryEntityDelta>,
}

/// One entity's index changes: attribute values to add and to remove. A transfer,
/// for example, removes the old `$owner` value and adds the new one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuxiliaryEntityDelta {
    /// The entity, by its index id.
    pub entity_id: u64,
    /// Values to add to the index.
    pub inserts: Vec<AttrEntry>,
    /// Values to remove from the index.
    pub removes: Vec<AttrEntry>,
}

/// One `(attribute, value)` pair in the index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttrEntry {
    pub attr: Vec<u8>,
    pub value: Vec<u8>,
}
