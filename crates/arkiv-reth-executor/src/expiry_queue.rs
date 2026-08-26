//! Process-local expiry schedule used by the blessed payload builder.

use alloy_primitives::B256;
use arkiv_interfaces::gas::purge_cost;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{OnceLock, RwLock},
};

#[derive(Debug, Default)]
pub struct ExpiryQueue {
    by_expiry: BTreeMap<u64, BTreeSet<B256>>,
    by_key: HashMap<B256, ScheduledExpiry>,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledExpiry {
    expires_at: u64,
    attribute_count: usize,
}

impl ExpiryQueue {
    pub fn insert(&mut self, key: B256, expires_at: u64, attribute_count: usize) {
        self.remove(key);
        self.by_expiry.entry(expires_at).or_default().insert(key);
        self.by_key.insert(
            key,
            ScheduledExpiry {
                expires_at,
                attribute_count,
            },
        );
    }

    pub fn remove(&mut self, key: B256) {
        let Some(scheduled) = self.by_key.remove(&key) else {
            return;
        };
        if let Some(keys) = self.by_expiry.get_mut(&scheduled.expires_at) {
            keys.remove(&key);
            if keys.is_empty() {
                self.by_expiry.remove(&scheduled.expires_at);
            }
        }
    }

    pub fn take_expired(&mut self, block: u64, limit: usize) -> Vec<B256> {
        let keys = self.expired(block, limit);
        for key in &keys {
            self.remove(*key);
        }
        keys
    }

    pub fn expired(&self, block: u64, limit: usize) -> Vec<B256> {
        self.by_expiry
            .range(..=block)
            .flat_map(|(_, keys)| keys.iter().copied())
            .take(limit)
            .collect()
    }

    /// Chronological selection with a soft gas threshold: the entity that crosses
    /// the threshold is included, then selection stops. This lets one unusually
    /// expensive entity make progress even when it exceeds the threshold alone.
    pub fn select_expired(&self, block: u64, limit: usize, gas_threshold: u64) -> Vec<B256> {
        let mut selected = Vec::with_capacity(limit);
        let mut gas = 0u64;
        for key in self.by_expiry.range(..=block).flat_map(|(_, keys)| keys) {
            if selected.len() == limit {
                break;
            }
            let scheduled = self
                .by_key
                .get(key)
                .expect("expiry index and metadata agree");
            gas = gas.saturating_add(purge_cost(scheduled.attribute_count));
            selected.push(*key);
            if gas > gas_threshold {
                break;
            }
        }
        selected
    }
}

static EXPIRY_QUEUE: OnceLock<RwLock<ExpiryQueue>> = OnceLock::new();

pub fn expiry_queue() -> &'static RwLock<ExpiryQueue> {
    EXPIRY_QUEUE.get_or_init(Default::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_by_expiry_then_key_and_reschedules() {
        let mut q = ExpiryQueue::default();
        q.insert(B256::repeat_byte(3), 8, 0);
        q.insert(B256::repeat_byte(2), 7, 0);
        q.insert(B256::repeat_byte(1), 7, 0);
        q.insert(B256::repeat_byte(3), 6, 0);
        assert_eq!(
            q.take_expired(7, 2),
            vec![B256::repeat_byte(3), B256::repeat_byte(1)]
        );
        assert_eq!(q.take_expired(8, 10), vec![B256::repeat_byte(2)]);
    }

    #[test]
    fn threshold_includes_the_crossing_entity_then_stops() {
        let mut q = ExpiryQueue::default();
        for byte in 0..10 {
            q.insert(B256::repeat_byte(byte), 7, 32); // 170k each
        }
        let selected = q.select_expired(7, 10, 1_000_000);
        assert_eq!(selected.len(), 6); // 1.02m: the sixth crosses the threshold
    }

    #[test]
    fn one_entity_can_exceed_the_threshold() {
        let mut q = ExpiryQueue::default();
        q.insert(B256::repeat_byte(1), 7, 300);
        q.insert(B256::repeat_byte(2), 7, 0);
        assert_eq!(
            q.select_expired(7, 10, 1_000_000),
            vec![B256::repeat_byte(1)]
        );
    }
}
