//! [`EqualityIndex`] — the **equality half** of the index (tier 1): one
//! roaring [`Bitmap`] per `(attribute, value)` pair, stored as the code of the
//! pair's derived account ([`pair_address`]).
//!
//! This is the whole index for equality queries, and the *resolution target*
//! for everything else: a [`RangeIndex`](crate::indices::range_index::RangeIndex)
//! scan answers in **values**, each of which resolves back to its bitmap here.
//! The `($all, "")` bucket doubles as the set of every live entity.

use alloy_primitives::Address;
use arkiv_interfaces::entity::AttributeType;

use crate::entities::AccountCode;
use crate::indices::address::{all_entities_bucket, pair_address};
use crate::indices::bitmap::Bitmap;
use crate::indices::error::AuxError;

/// Tier 1: the per-`(attribute, value)` entity bitmaps, over an [`AccountCode`]
/// backend.
pub(crate) struct EqualityIndex<B> {
    backend: B,
}

impl<B, E> EqualityIndex<B>
where
    B: AccountCode<Error = E>,
{
    pub(crate) const fn new(backend: B) -> Self {
        Self { backend }
    }

    /// The bitmap stored at `addr` (empty if the account has no code).
    pub(crate) fn bucket(&mut self, addr: Address) -> Result<Bitmap, AuxError<E>> {
        let code = self.backend.code(addr).map_err(AuxError::Backend)?;
        if code.is_empty() {
            Ok(Bitmap::new())
        } else {
            Ok(Bitmap::from_bytes(&code)?)
        }
    }

    /// The entities carrying `(attr, value)` — `value_bytes` being the value's
    /// [`index_bytes`](arkiv_interfaces::entity::AttributeValue::index_bytes).
    pub(crate) fn get(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        value_bytes: &[u8],
    ) -> Result<Bitmap, AuxError<E>> {
        self.bucket(pair_address(attr, ty, value_bytes))
    }

    /// Every live entity: the `($all, "")` bucket.
    pub(crate) fn all_entities(&mut self) -> Result<Bitmap, AuxError<E>> {
        self.bucket(all_entities_bucket())
    }

    /// Add `entity_id` to the `(attr, value)` bitmap. Returns whether the value
    /// was **newly present** (its bitmap was empty before) — the signal that
    /// the [`RangeIndex`](crate::indices::range_index::RangeIndex) must now
    /// record the value.
    pub(crate) fn insert(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        value_bytes: &[u8],
        entity_id: u64,
    ) -> Result<bool, AuxError<E>> {
        let addr = pair_address(attr, ty, value_bytes);
        let mut bitmap = self.bucket(addr)?;
        let was_empty = bitmap.is_empty();
        bitmap.insert(entity_id);
        self.backend
            .set_code(addr, bitmap.to_bytes())
            .map_err(AuxError::Backend)?;
        Ok(was_empty)
    }

    /// Remove `entity_id` from the `(attr, value)` bitmap. Returns whether the
    /// value is **now absent** (this removal emptied its bitmap) — the signal
    /// that the [`RangeIndex`](crate::indices::range_index::RangeIndex) must
    /// drop the value. A remove against an already-empty bitmap is a no-op.
    pub(crate) fn remove(
        &mut self,
        attr: &[u8],
        ty: AttributeType,
        value_bytes: &[u8],
        entity_id: u64,
    ) -> Result<bool, AuxError<E>> {
        let addr = pair_address(attr, ty, value_bytes);
        let mut bitmap = self.bucket(addr)?;
        if bitmap.is_empty() {
            return Ok(false);
        }
        bitmap.remove(entity_id);
        self.backend
            .set_code(addr, bitmap.to_bytes())
            .map_err(AuxError::Backend)?;
        Ok(bitmap.is_empty())
    }
}
