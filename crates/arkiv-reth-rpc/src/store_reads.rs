//! The `arkiv_*` read path, answered from the GolemDB store.
//!
//! Replaces the MPT read path (`SnapshotAccountCode` over a reth state
//! snapshot, entities decoded out of account code, the index walked as storage
//! slots). An entity is a record, so a read is a point `get`; a query is the
//! store [`Filter`] of the parsed query, node for node, and one `query` call.
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

/// Why a store read failed.
#[derive(Debug)]
pub enum ReadError {
    Store(StoreError),
    Record(arkiv_interfaces::entity_records::RecordError),
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e:?}"),
            Self::Record(e) => write!(f, "record: {e:?}"),
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

/// Entities matching `query`, as of `at`, one page at a time.
pub fn query<S: Store + ?Sized>(
    store: &S,
    at: CommitId,
    query: &ArkivQuery,
    offset: u64,
    limit: u64,
) -> Result<Vec<Entity>, ReadError> {
    let result = store
        .query(
            Some(at),
            &Query {
                filter: Filter::from(query),
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
    Ok(store
        .count(Some(at), &Filter::from(query), None)?
        .into_value())
}
