//! The two state stores and the per-block deltas that change them.

use alloc::vec::Vec;

use crate::entity::{AttributeValue, Entity};
use crate::primitives::{BlockNumber, EntityAddress, Hash};
use crate::query::{PageParams, Query, QueryMatches};

/// Holds the **entities**: a map from [`EntityAddress`] to [`Entity`], plus a
/// [`commitment`](EntityStore::commitment) over the whole map.
///
/// The store deals in whole [`Entity`] values — how it serializes them, and where
/// the host puts them, are its own concern behind this trait.
///
/// Every method takes `&mut self`, reads included: a host's read path may need to
/// update a cache.
pub trait EntityStore {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// This entity, or `None` if there is no such entity.
    fn get(&mut self, entity: EntityAddress) -> Result<Option<Entity>, Self::Error>;

    /// Apply one block's entity changes: the writes and removals in `delta`.
    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Self::Error>;

    /// Commitment over every entity. Separate from the index's
    /// ([`AuxiliaryStore::commitment`]).
    fn commitment(&mut self) -> Result<Hash, Self::Error>;
}

/// Optional: read entities as of a **past block**, for a host that keeps — or can
/// reconstruct — history. Extends [`EntityStore`] (whose reads are at the tip).
pub trait HistoricalEntityStore: EntityStore {
    /// This entity as of block `at`, or `None` if it didn't exist then.
    fn get_at(
        &mut self,
        entity: EntityAddress,
        at: BlockNumber,
    ) -> Result<Option<Entity>, Self::Error>;
}

/// Holds the **query index** over the entities.
///
/// Where the [`EntityStore`] is a plain key→entity map, this store understands the
/// query language: give it a [`Query`] and it returns the keys of the entities that
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
    /// Entities written this block. Each carries its own [`EntityAddress`]; the store
    /// serializes them however it likes.
    pub puts: Vec<Entity>,
    /// Entities removed this block.
    pub deletes: Vec<EntityAddress>,
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
///
/// The entity is named only by its [`EntityAddress`]. The index's own compact `u64`
/// id — what its bitmaps are keyed on — is the store's concern: it maps key → id
/// itself (allocating on the key's first appearance), so a producer of deltas never
/// deals in ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuxiliaryEntityDelta {
    /// The entity whose index entries are changing.
    pub entity_key: EntityAddress,
    /// Values to add to the index.
    pub inserts: Vec<AttrEntry>,
    /// Values to remove from the index.
    pub removes: Vec<AttrEntry>,
}

/// One `(attribute, value)` pair in the index.
///
/// The value is a typed [`AttributeValue`], not loose bytes, because the index
/// needs its type twice over: to decide whether — and how — the value is *ordered*
/// for range queries (a [`U256`](AttributeValue::U256) numerically, a
/// [`Str`](AttributeValue::Str) lexically, an
/// [`EntityAddress`](AttributeValue::EntityKey) not at all), and to keep the buckets of
/// different types disjoint. The bytes it is actually keyed on are
/// [`AttributeValue::index_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrEntry {
    pub attr: Vec<u8>,
    pub value: AttributeValue,
}

impl AttrEntry {
    /// An index entry for `attr` holding `value`.
    pub fn new(attr: impl Into<Vec<u8>>, value: AttributeValue) -> Self {
        Self {
            attr: attr.into(),
            value,
        }
    }
}
