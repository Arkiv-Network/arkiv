//! The index **write path**: fold one entity's `(attr, value)` change into the
//! tier-1 bitmap and, where the attribute is ordered, the tier-2 range structure.
//!
//! Ported from `arkiv-db-engine`'s `insert_into_indexes`/`remove_from_indexes`. The
//! shape to keep in mind:
//!
//! - **Tier-1 is per entity.** Every change reads the pair bitmap, adds or removes
//!   this entity's id, and writes it back as the pair account's code.
//! - **Tier-2 is per distinct value.** The ordered index records which *values*
//!   exist, not which entities carry them, so it is only touched when a value's
//!   bitmap crosses the empty boundary: an insert that makes a value first appear
//!   ([`was_empty`]) adds it to the tier-2 index; a remove that empties a value's
//!   bitmap drops it. Equality-only attributes ([`QueryCapabilities::Equality`]) have no tier-2.
//!
//! [`was_empty`]: insert

use alloy_primitives::Address;
use arkiv_interfaces::constants::WORD_LEN;
use arkiv_interfaces::entity::{AttributeType, AttributeValue};
use arkiv_reth_entitystore::AccountCode;

use crate::address::pair_address;
use crate::annotation::QueryCapabilities;
use crate::bitmap::Bitmap;
use crate::cascade::{self, MAX_STR_BYTES};
use crate::error::AuxError;
use crate::range;
use crate::storage::IndexStorage;

/// Read the pair bitmap at `pair_addr` (empty if the account has no code).
pub(crate) fn read_pair_bitmap<B, E>(
    backend: &mut B,
    pair_addr: Address,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    let code = backend.code(pair_addr).map_err(AuxError::Backend)?;
    if code.is_empty() {
        Ok(Bitmap::new())
    } else {
        Ok(Bitmap::from_bytes(&code)?)
    }
}

/// Add `entity_id` to the `(attr, value)` pair: set the tier-1 bitmap, and if this
/// value is newly present, record it in the tier-2 index for `mode`.
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
    let pair_addr = pair_address(attr, value.attr_type(), &bytes);
    let mut bitmap = read_pair_bitmap(backend, pair_addr)?;
    let was_empty = bitmap.is_empty();
    bitmap.insert(entity_id);
    backend
        .set_code(pair_addr, bitmap.to_bytes())
        .map_err(AuxError::Backend)?;
    if was_empty {
        tier2_insert(backend, attr, value.attr_type(), &bytes, capabilities)?;
    }
    Ok(())
}

/// Remove `entity_id` from the `(attr, value)` pair: clear it from the tier-1
/// bitmap, and if that empties the value, drop it from the tier-2 index for `mode`.
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
    let pair_addr = pair_address(attr, value.attr_type(), &bytes);
    let mut bitmap = read_pair_bitmap(backend, pair_addr)?;
    if bitmap.is_empty() {
        return Ok(());
    }
    bitmap.remove(entity_id);
    backend
        .set_code(pair_addr, bitmap.to_bytes())
        .map_err(AuxError::Backend)?;
    if bitmap.is_empty() {
        tier2_remove(backend, attr, value.attr_type(), &bytes, capabilities)?;
    }
    Ok(())
}

/// Record `value` in `attr`'s ordered tier-2 index. A no-op for
/// [`QueryCapabilities::Equality`], or for a value too long for its mode's structure — such a
/// value stays equality-indexed (upstream validation keeps values within these
/// limits; the guard just avoids a panic if one slips through).
fn tier2_insert<B, E>(
    backend: &mut B,
    attr: &[u8],
    ty: AttributeType,
    value: &[u8],
    capabilities: QueryCapabilities,
) -> Result<(), AuxError<E>>
where
    B: IndexStorage<Error = E>,
{
    match capabilities {
        QueryCapabilities::EqualityAndRange if value.len() <= WORD_LEN => {
            range::insert(backend, attr, ty, value).map_err(AuxError::Backend)
        }
        QueryCapabilities::EqualityAndPrefix if value.len() <= MAX_STR_BYTES => {
            cascade::insert(backend, attr, ty, value).map_err(AuxError::Backend)
        }
        _ => Ok(()),
    }
}

/// Drop `value` from `attr`'s ordered tier-2 index — the inverse of
/// [`tier2_insert`], with the same mode/length guard.
fn tier2_remove<B, E>(
    backend: &mut B,
    attr: &[u8],
    ty: AttributeType,
    value: &[u8],
    capabilities: QueryCapabilities,
) -> Result<(), AuxError<E>>
where
    B: IndexStorage<Error = E>,
{
    match capabilities {
        QueryCapabilities::EqualityAndRange if value.len() <= WORD_LEN => {
            range::remove(backend, attr, ty, value).map_err(AuxError::Backend)
        }
        QueryCapabilities::EqualityAndPrefix if value.len() <= MAX_STR_BYTES => {
            cascade::remove(backend, attr, ty, value).map_err(AuxError::Backend)
        }
        _ => Ok(()),
    }
}
