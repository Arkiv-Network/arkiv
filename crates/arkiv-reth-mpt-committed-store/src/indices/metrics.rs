//! Bounded, process-local telemetry for equality bitmap rewrites.
//!
//! These observations describe successful `AccountCode::set_code` calls,
//! including speculative execution and replay. They are neither committed
//! state totals nor physical database writes. Nothing here enters consensus.
//! Attribute values, entity ids, and bucket addresses never become labels.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use metrics::{SharedString, counter, describe_counter, describe_gauge, gauge};

const MAX_USER_ATTRIBUTES: usize = 64;
const MAX_ATTRIBUTE_BYTES: usize = 128;
// `$` is reserved by the protocol, so a user attribute cannot collide with
// this aggregate. Keep built-ins even when the user-name budget is exhausted.
const OTHER: &str = "$other";
const BUILT_INS: [&str; 7] = [
    "$all",
    "$creator",
    "$createdAtBlock",
    "$owner",
    "$key",
    "$expiration",
    "$contentType",
];

#[derive(Clone, Copy, Default)]
pub(super) struct BucketObservation {
    pub bytes: usize,
    pub members: u64,
}

struct AttributeMetrics {
    label: SharedString,
    largest: Mutex<BucketObservation>,
}

impl AttributeMetrics {
    fn new(label: SharedString) -> Self {
        Self {
            label,
            largest: Mutex::new(BucketObservation::default()),
        }
    }

    fn record(&self, before: BucketObservation, after: BucketObservation) {
        // Register against the current recorder: caching handles before reth
        // installs its recorder would permanently cache no-op handles.
        counter!("arkiv_index_equality_rewrites_total", "attribute" => self.label.clone())
            .increment(1);
        counter!("arkiv_index_equality_rewritten_bytes_total", "attribute" => self.label.clone())
            .increment(after.bytes as u64);
        counter!("arkiv_index_equality_loaded_bytes_total", "attribute" => self.label.clone())
            .increment(before.bytes as u64);
        counter!("arkiv_index_equality_growth_bytes_total", "attribute" => self.label.clone())
            .increment(after.bytes.saturating_sub(before.bytes) as u64);
        counter!("arkiv_index_equality_shrink_bytes_total", "attribute" => self.label.clone())
            .increment(before.bytes.saturating_sub(after.bytes) as u64);
        counter!("arkiv_index_equality_members_added_total", "attribute" => self.label.clone())
            .increment(after.members.saturating_sub(before.members));
        counter!("arkiv_index_equality_members_removed_total", "attribute" => self.label.clone())
            .increment(before.members.saturating_sub(after.members));

        // Serialize the two gauge updates per attribute so a slower writer
        // cannot publish an older maximum after a newer writer. Poisoned
        // telemetry locks must not turn an otherwise valid block into an error.
        let mut largest = self.largest.lock().unwrap_or_else(|e| e.into_inner());
        largest.bytes = largest.bytes.max(before.bytes).max(after.bytes);
        largest.members = largest.members.max(before.members).max(after.members);
        gauge!("arkiv_index_equality_largest_observed_bucket_bytes", "attribute" => self.label.clone())
            .set(largest.bytes as f64);
        gauge!("arkiv_index_equality_largest_observed_bucket_members", "attribute" => self.label.clone())
            .set(largest.members as f64);
    }
}

struct Registry {
    attributes: RwLock<HashMap<Vec<u8>, Arc<AttributeMetrics>>>,
    other: Arc<AttributeMetrics>,
}

impl Registry {
    fn new() -> Self {
        describe();
        Self {
            attributes: RwLock::new(
                BUILT_INS
                    .into_iter()
                    .map(|name| {
                        (
                            name.as_bytes().to_vec(),
                            Arc::new(AttributeMetrics::new(SharedString::const_str(name))),
                        )
                    })
                    .collect(),
            ),
            other: Arc::new(AttributeMetrics::new(SharedString::const_str(OTHER))),
        }
    }

    fn attribute(&self, attr: &[u8]) -> Arc<AttributeMetrics> {
        {
            let attributes = self.attributes.read().unwrap_or_else(|e| e.into_inner());
            if let Some(metrics) = attributes.get(attr) {
                return metrics.clone();
            }
        }
        // Reject before allocation. Unknown reserved names, invalid UTF-8, and
        // overlong names share the same bounded overflow series.
        if attr.is_empty() || attr.len() > MAX_ATTRIBUTE_BYTES || attr[0] == b'$' {
            return self.other.clone();
        }
        let Ok(name) = std::str::from_utf8(attr) else {
            return self.other.clone();
        };
        let mut attributes = self.attributes.write().unwrap_or_else(|e| e.into_inner());
        // A concurrent writer may have registered this name after our read.
        if let Some(metrics) = attributes.get(attr) {
            return metrics.clone();
        }
        if attributes.len() == BUILT_INS.len() + MAX_USER_ATTRIBUTES {
            return self.other.clone();
        }
        // Every observation reuses this label in several metric keys. Share
        // its allocation so cloning those keys never copies the name bytes.
        let metrics = Arc::new(AttributeMetrics::new(SharedString::from_shared(Arc::from(
            name,
        ))));
        attributes.insert(attr.to_vec(), metrics.clone());
        metrics
    }
}

pub(super) fn record_rewrite(attr: &[u8], before: BucketObservation, after: BucketObservation) {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY
        .get_or_init(Registry::new)
        .attribute(attr)
        .record(before, after);
}

fn describe() {
    describe_counter!(
        "arkiv_index_equality_rewrites_total",
        "Successful equality bitmap set_code calls during execution; includes speculative work and replay."
    );
    describe_counter!(
        "arkiv_index_equality_rewritten_bytes_total",
        "Serialized bitmap bytes passed to successful set_code calls; not committed or physical disk bytes."
    );
    describe_counter!(
        "arkiv_index_equality_loaded_bytes_total",
        "Original serialized bitmap bytes loaded for successful rewrites; not physical disk reads."
    );
    describe_counter!(
        "arkiv_index_equality_growth_bytes_total",
        "Sum of positive serialized size deltas across successful execution rewrites."
    );
    describe_counter!(
        "arkiv_index_equality_shrink_bytes_total",
        "Sum of negative serialized size deltas, as positive bytes, across successful execution rewrites."
    );
    describe_counter!(
        "arkiv_index_equality_members_added_total",
        "Entity memberships actually added by successful execution rewrites."
    );
    describe_counter!(
        "arkiv_index_equality_members_removed_total",
        "Entity memberships actually removed by successful execution rewrites."
    );
    describe_gauge!(
        "arkiv_index_equality_largest_observed_bucket_bytes",
        "Largest single serialized bucket seen before or after a successful rewrite since process start; not total index size."
    );
    describe_gauge!(
        "arkiv_index_equality_largest_observed_bucket_members",
        "Largest single bucket membership count seen before or after a successful rewrite since process start."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use metrics_util::layers::{PrefixLayer, Stack};

    #[test]
    fn names_are_bounded_builtins_survive_and_overflow_does_not_evict() {
        let registry = Registry::new();
        for name in ["ds", "kind", "chainid"] {
            assert_eq!(registry.attribute(name.as_bytes()).label.as_ref(), name);
        }
        for i in 3..MAX_USER_ATTRIBUTES {
            let name = format!("attribute_{i}");
            assert_eq!(registry.attribute(name.as_bytes()).label.as_ref(), name);
        }
        for i in 0..1_000 {
            assert_eq!(
                registry
                    .attribute(format!("overflow_{i}").as_bytes())
                    .label
                    .as_ref(),
                OTHER
            );
        }
        for name in BUILT_INS {
            assert_eq!(registry.attribute(name.as_bytes()).label.as_ref(), name);
        }
        assert_eq!(registry.attribute(b"kind").label.as_ref(), "kind");
        assert_eq!(
            registry.attributes.read().unwrap().len(),
            MAX_USER_ATTRIBUTES + BUILT_INS.len()
        );
    }

    #[test]
    fn unrepresentable_names_share_overflow_without_consuming_slots() {
        let registry = Registry::new();
        for name in [
            &b""[..],
            &b"\xff"[..],
            &b"$other"[..],
            &b"$unknown"[..],
            &[b'a'; MAX_ATTRIBUTE_BYTES + 1][..],
        ] {
            assert_eq!(registry.attribute(name).label.as_ref(), OTHER);
        }
        assert_eq!(registry.attributes.read().unwrap().len(), BUILT_INS.len());
    }

    #[test]
    fn exporter_uses_reth_prefix_and_tracks_growth_shrink_and_high_water() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let recorder = Stack::new(recorder).push(PrefixLayer::new("reth"));
        metrics::with_local_recorder(&recorder, || {
            let registry = Registry::new();
            let attribute = registry.attribute(b"kind");
            let empty = BucketObservation::default();
            let big = BucketObservation {
                bytes: 100,
                members: 20,
            };
            let small = BucketObservation {
                bytes: 60,
                members: 10,
            };
            attribute.record(empty, big);
            attribute.record(big, small);
            attribute.record(small, small);
        });
        let output = handle.render();
        for (name, value) in [
            ("rewrites_total", 3),
            ("rewritten_bytes_total", 220),
            ("loaded_bytes_total", 160),
            ("growth_bytes_total", 100),
            ("shrink_bytes_total", 40),
            ("members_added_total", 20),
            ("members_removed_total", 10),
            ("largest_observed_bucket_bytes", 100),
            ("largest_observed_bucket_members", 20),
        ] {
            let sample = format!("reth_arkiv_index_equality_{name}{{attribute=\"kind\"}} {value}");
            assert!(
                output.lines().any(|line| line == sample),
                "{sample}\n{output}"
            );
        }
        assert!(output.contains("# TYPE reth_arkiv_index_equality_rewritten_bytes_total counter"));
        assert!(
            output.contains("# TYPE reth_arkiv_index_equality_largest_observed_bucket_bytes gauge")
        );
    }
}
