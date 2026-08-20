//! [`RangeIndex`] — the **ordered half** of the index (tier 2): which *values*
//! exist per attribute, in an order that supports scans.
//!
//! Where the [`EqualityIndex`](crate::indices::equality_index::EqualityIndex)
//! is per entity (who carries this value), the range index is per **distinct
//! value** (which values exist at all) — it holds no entity ids. A scan
//! enumerates the values matching a bound or prefix; the caller resolves each
//! back to its equality bitmap.
//!
//! Two structures back it, chosen by the attribute's [`QueryCapabilities`]:
//! values ≤ [`EVM_WORD_LENGTH`] with range capability go through the storage-slot
//! B-tree ([`range`](crate::indices::range)); strings with prefix capability go
//! through the [`cascade`](crate::indices::cascade). Equality-only attributes
//! have no tier 2 at all — inserts and scans are no-ops for them.

use arkiv_interfaces::constants::EVM_WORD_LENGTH;
use arkiv_interfaces::entity::AttributeType;

use crate::indices::annotation::QueryCapabilities;
use crate::indices::cascade::{self, MAX_STR_BYTES};
use crate::indices::error::AuxError;
use crate::indices::range::{self, Bound};
use crate::indices::storage::IndexStorage;

/// Tier 2: the ordered per-attribute value index, over an [`IndexStorage`]
/// backend.
pub(crate) struct RangeIndex<B> {
    backend: B,
}

impl<B, E> RangeIndex<B>
where
    B: IndexStorage<Error = E>,
{
    pub(crate) const fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Record `value_bytes` in `attr`'s ordered structure. A no-op for
    /// equality-only capabilities, or for a value too long for its mode's
    /// structure — such a value stays equality-indexed (upstream validation
    /// keeps values within these limits; the guard just avoids a panic if one
    /// slips through).
    pub(crate) fn insert(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        value_bytes: &[u8],
        capabilities: QueryCapabilities,
    ) -> Result<(), AuxError<E>> {
        match capabilities {
            QueryCapabilities::EqualityAndRange if value_bytes.len() <= EVM_WORD_LENGTH => {
                range::insert(&mut self.backend, attr, ty, value_bytes).map_err(AuxError::Backend)
            }
            QueryCapabilities::EqualityAndPrefix if value_bytes.len() <= MAX_STR_BYTES => {
                cascade::insert(&mut self.backend, attr, ty, value_bytes).map_err(AuxError::Backend)
            }
            _ => Ok(()),
        }
    }

    /// Drop `value_bytes` from `attr`'s ordered structure — the inverse of
    /// [`insert`](Self::insert), with the same capability/length guard.
    pub(crate) fn remove(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        value_bytes: &[u8],
        capabilities: QueryCapabilities,
    ) -> Result<(), AuxError<E>> {
        match capabilities {
            QueryCapabilities::EqualityAndRange if value_bytes.len() <= EVM_WORD_LENGTH => {
                range::remove(&mut self.backend, attr, ty, value_bytes).map_err(AuxError::Backend)
            }
            QueryCapabilities::EqualityAndPrefix if value_bytes.len() <= MAX_STR_BYTES => {
                cascade::remove(&mut self.backend, attr, ty, value_bytes).map_err(AuxError::Backend)
            }
            _ => Ok(()),
        }
    }

    /// Every recorded value of `attr` matching `bound` against `bound_bytes`.
    ///
    /// An attribute without an ordered structure answers the empty set — the
    /// parser rejects range queries over such attributes, so one arriving here
    /// simply matches nothing.
    pub(crate) fn scan(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        bound_bytes: &[u8],
        bound: Bound,
        capabilities: QueryCapabilities,
    ) -> Result<Vec<Vec<u8>>, AuxError<E>> {
        match capabilities {
            QueryCapabilities::EqualityAndRange => {
                range::scan(&mut self.backend, attr, ty, bound_bytes, bound)
                    .map_err(AuxError::Backend)
            }
            QueryCapabilities::EqualityAndPrefix => {
                cascade::scan(&mut self.backend, attr, ty, bound_bytes, bound)
                    .map_err(AuxError::Backend)
            }
            QueryCapabilities::Equality | QueryCapabilities::None => Ok(Vec::new()),
        }
    }

    /// Every recorded str-mode value of `attr` starting with `prefix`.
    pub(crate) fn get_prefix_matches(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>, AuxError<E>> {
        cascade::glob(&mut self.backend, attr, ty, prefix).map_err(AuxError::Backend)
    }
}
