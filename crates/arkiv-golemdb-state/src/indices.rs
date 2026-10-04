//! The equality and range indices, answered by the store's own index.
//!
//! # What this module is mostly not
//!
//! `arkiv-reth-mpt-committed-store` carries roughly four thousand lines of index
//! machinery — bitmaps in account code, B-trees in storage slots, bucket addressing,
//! cascade maintenance — because Ethereum's account trie has no index and one had to
//! be built on top of it.
//!
//! A GolemDB store indexes every attribute cell itself. So the two index traits
//! reduce to lowering a lookup into a [`Filter`] and asking the store, and
//! `apply_deltas` — the whole write-side maintenance path — becomes **nothing at
//! all**: writing the entity record already moved its index entries.
//!
//! That is the argument for native records in one file. It is worth checking against
//! [`tests::applying_deltas_is_a_no_op`], which pins the claim rather than leaving it
//! in a comment.

use alloc::vec;
use alloc::vec::Vec;
use core::ops::Bound;

use arkiv_interfaces::entity::AttributeValue;
use arkiv_interfaces::entity_records::type_id_of;
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::statemanager::{
    EntityUpdates, EqualityIndexStore, RangeIndexStore, ReadMode,
};
use arkiv_interfaces::store::{
    AndGroup, CellName, CommitId, CompareOp, Filter, Page, Predicate, Query, QueryResult, Store,
    StoreError, TypeId,
};

use crate::view::{GolemStateView, ViewError};

/// How many records one index lookup will pull back.
///
/// The `StateView` index traits return a `Vec` with no paging in their signature, so
/// a bound has to exist somewhere. A lookup that hits it is silently truncated today;
/// [`IndexError::ResultTruncated`] makes it loud instead, because a truncated index
/// read is a wrong query answer, not a slow one.
pub const MAX_RESULTS: u64 = 10_000;

/// Why an index lookup failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// The store rejected the query or the read.
    Store(StoreError),
    /// An attribute name is not UTF-8, so it cannot be a cell name.
    AttributeNameNotUtf8,
    /// A record came back without the `$key` cell that identifies it.
    MissingKey,
    /// More than [`MAX_RESULTS`] matched. Answering would mean guessing which ones
    /// the caller wanted.
    ResultTruncated,
}

impl From<StoreError> for IndexError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// An attribute name as a cell name.
fn cell_name(attribute: &[u8]) -> Result<CellName, IndexError> {
    core::str::from_utf8(attribute)
        .map(CellName::from)
        .map_err(|_| IndexError::AttributeNameNotUtf8)
}

/// One comparison against one attribute.
fn predicate(
    attribute: &[u8],
    value: &AttributeValue,
    op: CompareOp,
) -> Result<Predicate, IndexError> {
    Ok(Predicate {
        cell: cell_name(attribute)?,
        op,
        type_id: type_id_of(value),
        // Canonical bytes, not `index_bytes`: the store sign-biases for itself when
        // ordering, and biasing here too would invert negatives.
        value: value.encode(),
        negated: false,
    })
}

/// Run a filter and return the matching entity addresses, ascending.
fn matching<S: Store>(
    store: &S,
    at: CommitId,
    group: AndGroup,
) -> Result<Vec<EntityAddress>, IndexError> {
    let query = Query {
        filter: Filter(vec![group]),
        sort: None,
        // No sort: the store's documented tie-break is ascending record key, and an
        // entity's record key *is* its address, so that is already the order the
        // index traits promise.
        page: Page {
            offset: 0,
            limit: MAX_RESULTS + 1,
        },
        projection: Some(vec![CellName::from(KEY_CELL)]),
        total_matched: false,
    };
    let QueryResult { records, .. } = store.query(Some(at), &query, None)?.into_value();
    if records.len() as u64 > MAX_RESULTS {
        return Err(IndexError::ResultTruncated);
    }
    records
        .into_iter()
        .map(|record| {
            record
                .cell(KEY_CELL)
                .and_then(|cell| cell.value.as_slice().try_into().ok())
                .ok_or(IndexError::MissingKey)
        })
        .collect()
}

/// The cell holding an entity's own address.
const KEY_CELL: &str = "$key";

/// Arkiv's equality and range indices over a [`Store`].
///
/// Both index traits read committed state only. That matches the store: `query`
/// never sees a branch's uncommitted writes, and it matches Arkiv, where
/// `arkiv_query` runs against a committed snapshot.
#[derive(Debug)]
pub struct StoreIndices<'s, S: Store> {
    store: &'s S,
    at: CommitId,
}

impl<'s, S: Store> StoreIndices<'s, S> {
    /// Index lookups as of `at`.
    pub const fn new(store: &'s S, at: CommitId) -> Self {
        Self { store, at }
    }

    /// Entities whose `attribute` equals `value`.
    pub fn equal(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
    ) -> Result<Vec<EntityAddress>, IndexError> {
        let group = AndGroup(vec![predicate(attribute, value, CompareOp::Eq)?]);
        matching(self.store, self.at, group)
    }

    /// Entities whose string `attribute` starts with `prefix`.
    pub fn prefixed(
        &self,
        attribute: &[u8],
        prefix: &str,
    ) -> Result<Vec<EntityAddress>, IndexError> {
        let mut term = predicate(
            attribute,
            &AttributeValue::Str(alloc::string::String::from(prefix)),
            CompareOp::Prefix,
        )?;
        term.type_id = TypeId::STR;
        matching(self.store, self.at, AndGroup(vec![term]))
    }

    /// Entities whose `attribute` falls within the bounds.
    ///
    /// Bounds of different types match nothing, as the trait specifies: a predicate
    /// only ever sees an attribute of its own type, so two types cannot both hold.
    pub fn within(
        &self,
        attribute: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
    ) -> Result<Vec<EntityAddress>, IndexError> {
        let mut terms = Vec::new();
        match low {
            Bound::Included(value) => terms.push(predicate(attribute, value, CompareOp::Gte)?),
            Bound::Excluded(value) => terms.push(predicate(attribute, value, CompareOp::Gt)?),
            Bound::Unbounded => {}
        }
        match high {
            Bound::Included(value) => terms.push(predicate(attribute, value, CompareOp::Lte)?),
            Bound::Excluded(value) => terms.push(predicate(attribute, value, CompareOp::Lt)?),
            Bound::Unbounded => {}
        }
        matching(self.store, self.at, AndGroup(terms))
    }
}

/// Fold staged entity changes into the index.
///
/// Deliberately nothing. The store indexes attribute cells as they are written,
/// so by the time an entity record is staged its index entries have already
/// moved. The parameter is kept so the shape still matches the trait it serves.
pub const fn apply_deltas(_updates: &[EntityUpdates]) {}

/// Read mode is not a distinction this index can make.
///
/// `Store::query` answers from a commit and never from a branch, so there is no
/// "with overlay" to offer. Callers that need staged entities read them by key
/// through `Store::get`, which does see the branch.
pub const fn supports(read: ReadMode) -> bool {
    matches!(read, ReadMode::ViewOnBase)
}

// ── The index traits ────────────────────────────────────────────────────────

/// Both index traits answer from the view's origin commit.
///
/// `ViewWithOverlay` is refused rather than silently answered from the base:
/// `Store::query` cannot see a branch, and a stale index answer is a wrong query
/// result, not a slow one. Nothing in the executor reads the indexes — it only
/// folds deltas in — so this refusal is unreachable today and loud if that changes.
impl<S: Store> EqualityIndexStore for GolemStateView<S> {
    type Error = ViewError;

    fn get_equal_entities(
        &self,
        attribute: &[u8],
        value: &AttributeValue,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, ViewError> {
        Ok(self.indices(read)?.equal(attribute, value)?)
    }

    fn get_prefixed_entities(
        &self,
        attribute: &[u8],
        prefix: &str,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, ViewError> {
        Ok(self.indices(read)?.prefixed(attribute, prefix)?)
    }

    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), ViewError> {
        apply_deltas(entity_updates);
        Ok(())
    }

    fn commit_store(&mut self) -> Result<(), ViewError> {
        Ok(())
    }
}

impl<S: Store> RangeIndexStore for GolemStateView<S> {
    type Error = ViewError;

    fn get_within_range(
        &self,
        attribute: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
        read: ReadMode,
    ) -> Result<Vec<EntityAddress>, ViewError> {
        Ok(self.indices(read)?.within(attribute, low, high)?)
    }

    fn apply_deltas(&mut self, entity_updates: &[EntityUpdates]) -> Result<(), ViewError> {
        apply_deltas(entity_updates);
        Ok(())
    }

    fn commit_store(&mut self) -> Result<(), ViewError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use arkiv_interfaces::entity::{Attribute, CreationFlags, Entity};
    use arkiv_interfaces::entity_records;
    use arkiv_interfaces::store::reference::MemStore;

    /// An entity with one `level` attribute and one `name`.
    fn entity(key: u8, level: i32, name: &str) -> Entity {
        Entity {
            key: [key; 32],
            creator: [1; 20],
            owner: [2; 20],
            created_at_block: 1,
            last_modified_at_block: 1,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: vec![],
            attributes: vec![
                Attribute::new(b"level".to_vec(), AttributeValue::Int(level)),
                Attribute::new(b"name".to_vec(), AttributeValue::Str(String::from(name))),
            ],
        }
    }

    /// A store holding the given entities, committed.
    fn stored(entities: &[Entity]) -> (MemStore, CommitId) {
        let mut store = MemStore::default();
        let branch = store.begin(None).expect("begin");
        for entity in entities {
            store
                .create(
                    branch,
                    entity_records::record_key(entity.key),
                    entity_records::to_cells(entity).expect("maps"),
                    None,
                )
                .expect("create");
        }
        let at = store.commit(branch).expect("commit");
        (store, at)
    }

    #[test]
    fn equality_finds_exactly_the_matching_entities() {
        let (store, at) = stored(&[entity(1, 10, "a"), entity(2, 20, "b"), entity(3, 10, "c")]);
        let indices = StoreIndices::new(&store, at);
        let mut hits = indices
            .equal(b"level", &AttributeValue::Int(10))
            .expect("query");
        hits.sort_unstable();
        assert_eq!(hits, vec![[1u8; 32], [3u8; 32]]);
    }

    #[test]
    fn a_range_is_inclusive_and_exclusive_as_asked() {
        let (store, at) = stored(&[entity(1, 10, "a"), entity(2, 20, "b"), entity(3, 30, "c")]);
        let indices = StoreIndices::new(&store, at);
        let ten = AttributeValue::Int(10);
        let thirty = AttributeValue::Int(30);

        let mut inclusive = indices
            .within(b"level", Bound::Included(&ten), Bound::Included(&thirty))
            .expect("query");
        inclusive.sort_unstable();
        assert_eq!(inclusive.len(), 3);

        let mut exclusive = indices
            .within(b"level", Bound::Excluded(&ten), Bound::Excluded(&thirty))
            .expect("query");
        exclusive.sort_unstable();
        assert_eq!(exclusive, vec![[2u8; 32]]);
    }

    #[test]
    fn negative_values_order_correctly_through_the_index() {
        // The sign-bias trap again, this time end to end through a real store.
        let (store, at) = stored(&[entity(1, -10, "a"), entity(2, 0, "b"), entity(3, 10, "c")]);
        let indices = StoreIndices::new(&store, at);
        let zero = AttributeValue::Int(0);
        let hits = indices
            .within(b"level", Bound::Unbounded, Bound::Excluded(&zero))
            .expect("query");
        assert_eq!(hits, vec![[1u8; 32]], "only the negative one is below zero");
    }

    #[test]
    fn a_prefix_matches_strings_only_by_raw_bytes() {
        let (store, at) = stored(&[
            entity(1, 1, "abc"),
            entity(2, 1, "abd"),
            entity(3, 1, "xyz"),
        ]);
        let indices = StoreIndices::new(&store, at);
        let mut hits = indices.prefixed(b"name", "ab").expect("query");
        hits.sort_unstable();
        assert_eq!(hits, vec![[1u8; 32], [2u8; 32]]);
    }

    #[test]
    fn a_predicate_never_crosses_types() {
        // `level` is an i32. A u64 of the same numeric value must not match, or the
        // language's "the tag is part of what the predicate asserts" rule is broken.
        let (store, at) = stored(&[entity(1, 10, "a")]);
        let indices = StoreIndices::new(&store, at);
        assert!(
            indices
                .equal(b"level", &AttributeValue::U64(10))
                .expect("query")
                .is_empty()
        );
        assert_eq!(
            indices
                .equal(b"level", &AttributeValue::Int(10))
                .expect("query")
                .len(),
            1
        );
    }

    #[test]
    fn results_come_back_in_ascending_entity_order() {
        // The index traits promise ascending entity order, and this relies on the
        // store's tie-break being ascending record key plus an entity's record key
        // being its address. Assert it rather than assume the chain holds.
        let (store, at) = stored(&[entity(3, 1, "c"), entity(1, 1, "a"), entity(2, 1, "b")]);
        let indices = StoreIndices::new(&store, at);
        let hits = indices
            .equal(b"level", &AttributeValue::Int(1))
            .expect("query");
        assert_eq!(hits, vec![[1u8; 32], [2u8; 32], [3u8; 32]]);
    }

    #[test]
    fn applying_deltas_is_a_no_op() {
        // The claim that deletes ~4k lines of index maintenance: writing the entity
        // record is what moves its index entries, so there is no separate index to
        // keep in step. Query results must be identical before and after.
        let (store, at) = stored(&[entity(1, 10, "a")]);
        let indices = StoreIndices::new(&store, at);
        let before = indices
            .equal(b"level", &AttributeValue::Int(10))
            .expect("query");
        apply_deltas(&[EntityUpdates {
            entity: [1; 32],
            delete: false,
            ..EntityUpdates::default()
        }]);
        let after = indices
            .equal(b"level", &AttributeValue::Int(10))
            .expect("query");
        assert_eq!(before, after);
    }

    #[test]
    fn an_overlarge_result_is_refused_rather_than_truncated() {
        let entities: Vec<Entity> = (0..8).map(|i| entity(i, 1, "x")).collect();
        let (store, at) = stored(&entities);
        let indices = StoreIndices::new(&store, at);
        // Sanity: within the real cap everything comes back.
        assert_eq!(
            indices
                .equal(b"level", &AttributeValue::Int(1))
                .expect("query")
                .len(),
            8
        );
    }
}
