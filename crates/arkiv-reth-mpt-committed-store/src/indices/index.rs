//! The index **write path**: fold one entity's `(attr, value)` change into the
//! [`EqualityIndex`] and, where the attribute is ordered, the [`RangeIndex`].
//!
//! Ported from `arkiv-db-engine`'s `insert_into_indexes`/`remove_from_indexes`.
//! The shape to keep in mind:
//!
//! - **The equality index is per entity.** Every change touches the pair
//!   bitmap, adding or removing this entity's id.
//! - **The range index is per distinct value.** It records which *values*
//!   exist, not which entities carry them, so it is only touched when a value's
//!   bitmap crosses the empty boundary: an insert that makes a value first
//!   appear adds it; a remove that empties a value's bitmap drops it. The
//!   crossing signal is [`EqualityIndex::insert`]/[`EqualityIndex::remove`]'s
//!   return value.

use arkiv_interfaces::entity::AttributeValue;

use crate::entities::AccountCode;
use crate::indices::annotation::QueryCapabilities;
use crate::indices::equality_index::EqualityIndex;
use crate::indices::error::AuxError;
use crate::indices::range_index::RangeIndex;
use crate::indices::storage::IndexStorage;

/// Add `entity_id` to the `(attr, value)` pair: set the equality bitmap, and if
/// this value is newly present, record it in the range index.
pub(crate) fn insert<B, E>(
    backend: &mut B,
    attr: &[u8],
    value: &AttributeValue,
    entity_id: u64,
    capabilities: QueryCapabilities,
) -> Result<(), AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    if capabilities == QueryCapabilities::None {
        return Ok(());
    }
    let bytes = value.index_bytes();
    let ty = value.attr_type();
    let newly_present = EqualityIndex::new(&mut *backend).insert(attr, ty, &bytes, entity_id)?;
    if newly_present {
        RangeIndex::new(backend).insert(attr, ty, &bytes, capabilities)?;
    }
    Ok(())
}

/// Remove `entity_id` from the `(attr, value)` pair: clear it from the equality
/// bitmap, and if that empties the value, drop it from the range index.
pub(crate) fn remove<B, E>(
    backend: &mut B,
    attr: &[u8],
    value: &AttributeValue,
    entity_id: u64,
    capabilities: QueryCapabilities,
) -> Result<(), AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    if capabilities == QueryCapabilities::None {
        return Ok(());
    }
    let bytes = value.index_bytes();
    let ty = value.attr_type();
    let now_absent = EqualityIndex::new(&mut *backend).remove(attr, ty, &bytes, entity_id)?;
    if now_absent {
        RangeIndex::new(backend).remove(attr, ty, &bytes, capabilities)?;
    }
    Ok(())
}
