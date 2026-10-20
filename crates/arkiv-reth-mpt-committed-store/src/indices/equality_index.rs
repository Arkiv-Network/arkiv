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
use crate::indices::metrics::{BucketObservation, record_rewrite};

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
        self.load_bucket(addr).map(|(bitmap, _)| bitmap)
    }

    /// Preserve the original byte length for telemetry, without reserializing
    /// the old bitmap or reading the account a second time.
    fn load_bucket(&mut self, addr: Address) -> Result<(Bitmap, usize), AuxError<E>> {
        let code = self.backend.code(addr).map_err(AuxError::Backend)?;
        let bitmap = if code.is_empty() {
            Bitmap::new()
        } else {
            Bitmap::from_bytes(&code)?
        };
        Ok((bitmap, code.len()))
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
        let (mut bitmap, old_bytes) = self.load_bucket(addr)?;
        let before = BucketObservation {
            bytes: old_bytes,
            members: bitmap.len(),
        };
        let was_empty = bitmap.is_empty();
        bitmap.insert(entity_id);
        self.write_bucket(attr, addr, &bitmap, before)?;
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
        let (mut bitmap, old_bytes) = self.load_bucket(addr)?;
        if bitmap.is_empty() {
            return Ok(false);
        }
        let before = BucketObservation {
            bytes: old_bytes,
            members: bitmap.len(),
        };
        bitmap.remove(entity_id);
        self.write_bucket(attr, addr, &bitmap, before)?;
        Ok(bitmap.is_empty())
    }

    fn write_bucket(
        &mut self,
        attr: &[u8],
        addr: Address,
        bitmap: &Bitmap,
        before: BucketObservation,
    ) -> Result<(), AuxError<E>> {
        let code = bitmap.to_bytes();
        let after = BucketObservation {
            bytes: code.len(),
            members: bitmap.len(),
        };
        self.backend
            .set_code(addr, code)
            .map_err(AuxError::Backend)?;
        // A successful overlay write is not a disk commit. Count the work even
        // if the enclosing execution is later discarded or replayed.
        record_rewrite(attr, before, after);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use metrics_util::layers::{PrefixLayer, Stack};
    use std::collections::HashMap;

    #[derive(Default)]
    struct Backend {
        code: HashMap<Address, Vec<u8>>,
        writes: usize,
        fail_write: bool,
    }

    impl AccountCode for Backend {
        type Error = &'static str;

        fn code(&mut self, addr: Address) -> Result<Vec<u8>, Self::Error> {
            Ok(self.code.get(&addr).cloned().unwrap_or_default())
        }

        fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Self::Error> {
            if self.fail_write {
                return Err("write failed");
            }
            self.writes += 1;
            self.code.insert(addr, code);
            Ok(())
        }

        fn clear_code(&mut self, addr: Address) -> Result<(), Self::Error> {
            self.code.remove(&addr);
            Ok(())
        }
    }

    #[test]
    fn instrumentation_preserves_bytes_and_counts_only_successful_rewrites() {
        const ATTR: &[u8] = b"telemetry_mutation_test";
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let recorder = Stack::new(recorder).push(PrefixLayer::new("reth"));
        let addr = pair_address(ATTR, AttributeType::Str, b"value");
        // Start from an existing/imported bucket: the first growth observation
        // must use its serialized length, not assume it was empty at startup.
        let initial = Bitmap::from_iter([1, 2]).to_bytes();
        let one = Bitmap::from_iter([1]).to_bytes();
        let empty = Bitmap::new().to_bytes();
        let mut backend = Backend::default();
        backend.code.insert(addr, initial.clone());
        metrics::with_local_recorder(&recorder, || {
            let mut index = EqualityIndex::new(&mut backend);
            assert_eq!(
                index.get(ATTR, AttributeType::Str, b"value").unwrap().len(),
                2
            );
            assert!(
                !handle.render().contains("telemetry_mutation_test"),
                "query reads must not register write metrics"
            );
            // These two operations still rewrite the whole bucket, although no
            // memberships or serialized size change.
            assert!(!index.insert(ATTR, AttributeType::Str, b"value", 1).unwrap());
            assert!(
                !index
                    .remove(ATTR, AttributeType::Str, b"value", 99)
                    .unwrap()
            );
            assert_eq!(index.backend.code.get(&addr).unwrap(), &initial);
            assert!(!index.remove(ATTR, AttributeType::Str, b"value", 2).unwrap());
            assert_eq!(index.backend.code.get(&addr).unwrap(), &one);
            assert!(index.remove(ATTR, AttributeType::Str, b"value", 1).unwrap());
            assert_eq!(index.backend.code.get(&addr).unwrap(), &empty);
            assert!(!index.remove(ATTR, AttributeType::Str, b"value", 1).unwrap());
            assert!(
                !index
                    .remove(ATTR, AttributeType::Str, b"absent", 1)
                    .unwrap()
            );
            assert_eq!(index.backend.writes, 4, "empty removals do not rewrite");

            index.backend.fail_write = true;
            assert!(index.insert(ATTR, AttributeType::Str, b"value", 3).is_err());
            assert_eq!(index.backend.code.get(&addr).unwrap(), &empty);
            index.backend.fail_write = false;
            assert!(index.insert(ATTR, AttributeType::Str, b"value", 1).unwrap());
            assert_eq!(index.backend.code.get(&addr).unwrap(), &one);
        });
        let output = handle.render();
        for (name, value) in [
            ("rewrites_total", 5),
            (
                "rewritten_bytes_total",
                2 * initial.len() + 2 * one.len() + empty.len(),
            ),
            (
                "loaded_bytes_total",
                3 * initial.len() + one.len() + empty.len(),
            ),
            ("growth_bytes_total", one.len() - empty.len()),
            ("shrink_bytes_total", initial.len() - empty.len()),
            ("members_added_total", 1),
            ("members_removed_total", 2),
            ("largest_observed_bucket_bytes", initial.len()),
            ("largest_observed_bucket_members", 2),
        ] {
            let sample = format!(
                "reth_arkiv_index_equality_{name}{{attribute=\"telemetry_mutation_test\"}} {value}"
            );
            assert!(
                output.lines().any(|line| line == sample),
                "{sample}\n{output}"
            );
        }
    }
}
