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

    /// Select expired entities in chronological order without exceeding `gas_limit`.
    pub fn select_expired(&self, block: u64, limit: usize, gas_limit: u64) -> Vec<B256> {
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
            let next_gas = gas.saturating_add(purge_cost(scheduled.attribute_count));
            if next_gas > gas_limit {
                break;
            }
            gas = next_gas;
            selected.push(*key);
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
            q.select_expired(7, 2, u64::MAX),
            vec![B256::repeat_byte(3), B256::repeat_byte(1)]
        );
        q.remove(B256::repeat_byte(3));
        q.remove(B256::repeat_byte(1));
        assert_eq!(
            q.select_expired(8, 10, u64::MAX),
            vec![B256::repeat_byte(2)]
        );
    }

    #[test]
    fn gas_limit_excludes_the_crossing_entity() {
        let mut q = ExpiryQueue::default();
        for byte in 0..10 {
            q.insert(B256::repeat_byte(byte), 7, 32); // 170k each
        }
        let selected = q.select_expired(7, 10, 1_000_000);
        assert_eq!(selected.len(), 5); // the sixth would raise the total to 1.02m
    }

    #[test]
    fn maximum_entity_purge_fits_the_gas_limit() {
        use arkiv_bindings::{MAX_ATTRIBUTES, PURGE_GAS_LIMIT};

        assert!(purge_cost(MAX_ATTRIBUTES) < PURGE_GAS_LIMIT);
    }
}
