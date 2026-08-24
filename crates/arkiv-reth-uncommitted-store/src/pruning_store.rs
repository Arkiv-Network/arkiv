//! [`MemPruningStore`] — the in-memory pruning set.

use std::collections::BTreeMap;

use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::statemanager::{PruningMeta, ReadMode};

/// An in-memory pruning set: committed entries plus a staged overlay. An add
/// is an upsert.
#[derive(Debug, Default, Clone)]
pub struct MemPruningStore {
    committed: BTreeMap<EntityAddress, PruningMeta>,
    /// `Some(meta)` = staged add/reorder; `None` = staged removal (a take).
    staged: BTreeMap<EntityAddress, Option<PruningMeta>>,
}

impl MemPruningStore {
    pub const fn new() -> Self {
        Self {
            committed: BTreeMap::new(),
            staged: BTreeMap::new(),
        }
    }

    pub fn add_to_pruning_set(&mut self, entity: EntityAddress, pruning_meta: PruningMeta) {
        self.staged.insert(entity, Some(pruning_meta));
    }

    pub fn peek_top(&self, top: u16, read: ReadMode) -> Vec<(EntityAddress, PruningMeta)> {
        let mut entries = self.merged(read);
        entries.truncate(top as usize);
        entries
    }

    pub fn take_top(&mut self, top: u16) -> Vec<(EntityAddress, PruningMeta)> {
        let taken = self.peek_top(top, ReadMode::ViewWithOverlay);
        for (entity, _) in &taken {
            self.staged.insert(*entity, None);
        }
        taken
    }

    pub fn is_dirty(&self) -> bool {
        !self.staged.is_empty()
    }

    pub fn commit_store(&mut self) {
        for (entity, staged) in core::mem::take(&mut self.staged) {
            match staged {
                Some(meta) => self.committed.insert(entity, meta),
                None => self.committed.remove(&entity),
            };
        }
    }

    /// Pruning order: [`PruningMeta`]'s `Ord`, ties by ascending entity.
    fn merged(&self, read: ReadMode) -> Vec<(EntityAddress, PruningMeta)> {
        let mut entries: Vec<(EntityAddress, PruningMeta)> = match read {
            ReadMode::ViewOnBase => self.committed.iter().map(|(k, m)| (*k, *m)).collect(),
            ReadMode::ViewWithOverlay => {
                let mut merged = self.committed.clone();
                for (entity, staged) in &self.staged {
                    match staged {
                        Some(meta) => merged.insert(*entity, *meta),
                        None => merged.remove(entity),
                    };
                }
                merged.into_iter().collect()
            }
        };
        entries.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(byte: u8) -> EntityAddress {
        [byte; 32]
    }

    fn meta(priority: u8, introduced_at: u64) -> PruningMeta {
        PruningMeta {
            priority,
            introduced_at,
        }
    }

    #[test]
    fn orders_by_priority_then_age_then_key() {
        let mut store = MemPruningStore::new();
        store.add_to_pruning_set(key_of(1), meta(0, 10));
        store.add_to_pruning_set(key_of(2), meta(9, 20));
        store.add_to_pruning_set(key_of(3), meta(9, 5));
        store.add_to_pruning_set(key_of(4), meta(9, 5));

        let order: Vec<_> = store
            .peek_top(10, ReadMode::ViewWithOverlay)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        // Priority 9 first; among those the older introduction; then key.
        assert_eq!(order, vec![key_of(3), key_of(4), key_of(2), key_of(1)]);
    }

    #[test]
    fn base_reads_skip_the_overlay_until_commit() {
        let mut store = MemPruningStore::new();
        store.add_to_pruning_set(key_of(1), meta(1, 10));

        assert!(store.peek_top(10, ReadMode::ViewOnBase).is_empty());
        assert_eq!(store.peek_top(10, ReadMode::ViewWithOverlay).len(), 1);

        store.commit_store();
        assert_eq!(store.peek_top(10, ReadMode::ViewOnBase).len(), 1);
    }

    #[test]
    fn take_removes_and_a_re_add_reorders() {
        let mut store = MemPruningStore::new();
        store.add_to_pruning_set(key_of(1), meta(1, 10));
        store.add_to_pruning_set(key_of(2), meta(5, 10));
        store.commit_store();

        let taken = store.take_top(1);
        assert_eq!(taken, vec![(key_of(2), meta(5, 10))]);
        assert_eq!(store.peek_top(10, ReadMode::ViewWithOverlay).len(), 1);
        // The take is staged, not committed.
        assert_eq!(store.peek_top(10, ReadMode::ViewOnBase).len(), 2);
        store.commit_store();
        assert_eq!(store.peek_top(10, ReadMode::ViewOnBase).len(), 1);

        // An add is an upsert: the entity moves, it doesn't duplicate.
        store.add_to_pruning_set(key_of(1), meta(255, 99));
        store.commit_store();
        assert_eq!(
            store.peek_top(10, ReadMode::ViewOnBase),
            vec![(key_of(1), meta(255, 99))]
        );
    }
}
