//! The `arkiv_*` read path, answered from the GolemDB store.
//!
//! Replaces the MPT read path (`SnapshotAccountCode` over a reth state
//! snapshot, entities decoded out of account code, the index walked as storage
//! slots). An entity is a record, so a read is a point `get`; a query is
//! [`arkiv_query::lower`] into a store [`Filter`] and one `query` call.
//!
//! # Paging
//!
//! `Store::query` pages by offset against a commit. A commit is immutable, so
//! an offset is as stable as the key-cursor the MPT path carried, and the
//! cursor stays opaque to the caller either way.

use arkiv_interfaces::entity::Entity;
use arkiv_interfaces::entity_records;
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::query::Query as ArkivQuery;
use arkiv_interfaces::store::{CommitId, Filter, Page, Query, ReadTarget, Store, StoreError};
use arkiv_query::lower::{self, Lowered};

/// Why a store read failed.
#[derive(Debug)]
pub enum ReadError {
    Store(StoreError),
    Record(arkiv_interfaces::entity_records::RecordError),
    /// The query cannot be answered by the store's index.
    NotIndexable(lower::LowerError),
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e:?}"),
            Self::Record(e) => write!(f, "record: {e:?}"),
            Self::NotIndexable(e) => write!(f, "query not answerable: {e:?}"),
        }
    }
}

impl From<StoreError> for ReadError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// One entity by key, as of `at`.
pub fn entity<S: Store + ?Sized>(
    store: &S,
    at: CommitId,
    key: EntityAddress,
) -> Result<Option<Entity>, ReadError> {
    let record = store
        .get(
            ReadTarget::Commit(at),
            entity_records::record_key(key),
            None,
            None,
        )?
        .into_value();
    record
        .as_ref()
        .map(entity_records::from_record)
        .transpose()
        .map_err(ReadError::Record)
}

/// Lower an Arkiv query to a store filter. `None` means "matches nothing",
/// which an empty store `Filter` cannot express — it means *everything*.
fn filter_of(query: &ArkivQuery) -> Result<Option<Filter>, ReadError> {
    match lower::lower(query).map_err(ReadError::NotIndexable)? {
        Lowered::Nothing => Ok(None),
        Lowered::Filter(filter) => Ok(Some(filter)),
    }
}

/// Entities matching `query`, as of `at`, one page at a time.
pub fn query<S: Store + ?Sized>(
    store: &S,
    at: CommitId,
    query: &ArkivQuery,
    offset: u64,
    limit: u64,
) -> Result<Vec<Entity>, ReadError> {
    let Some(filter) = filter_of(query)? else {
        return Ok(Vec::new());
    };
    let result = store
        .query(
            Some(at),
            &Query {
                filter,
                sort: None,
                page: Page { offset, limit },
                // The whole record: an entity is reconstructed from all of it.
                projection: None,
                total_matched: false,
            },
            None,
        )?
        .into_value();
    result
        .records
        .iter()
        .map(entity_records::from_record)
        .collect::<Result<Vec<_>, _>>()
        .map_err(ReadError::Record)
}

/// How many entities match, as of `at`.
pub fn count<S: Store + ?Sized>(
    store: &S,
    at: CommitId,
    query: &ArkivQuery,
) -> Result<u64, ReadError> {
    let Some(filter) = filter_of(query)? else {
        return Ok(0);
    };
    Ok(store.count(Some(at), &filter, None)?.into_value())
}
