//! [`MemPruningMap`] — the in-memory [`PruningMap`] store.
//!
//! Tracks which entities are tombstoned, when each is due for physical removal,
//! and how urgently ([`PruningPriority`]). The per-block purge works a bounded
//! budget; priority decides who gets it when more is due than fits — the higher
//! the priority, the closer to the top of the pruning stack.

use std::collections::BTreeMap;

use arkiv_interfaces::manager::{PruningMap, PruningPriority};
use arkiv_interfaces::primitives::{BlockNumber, EntityAddress};

/// An in-memory pruning map: entity → (due block, priority).
///
/// Keyed by entity so a reschedule is an upsert, and kept in a `BTreeMap` so
/// iteration is ascending-key deterministic — the tiebreak
/// [`pruning_due`](PruningMap::pruning_due) needs after its
/// highest-priority-first sort.
#[derive(Debug, Default, Clone)]
pub struct MemPruningMap {
    due: BTreeMap<EntityAddress, (BlockNumber, PruningPriority)>,
}

impl MemPruningMap {
    /// An empty map.
    pub const fn new() -> Self {
        Self {
            due: BTreeMap::new(),
        }
    }
}

impl PruningMap for MemPruningMap {
    type Error = core::convert::Infallible;

    fn schedule_pruning(
        &mut self,
        entity: EntityAddress,
        prune_at: BlockNumber,
        priority: PruningPriority,
    ) -> Result<(), Self::Error> {
        self.due.insert(entity, (prune_at, priority));
        Ok(())
    }

    fn pruning_due(&mut self, block: BlockNumber) -> Result<Vec<EntityAddress>, Self::Error> {
        let mut due: Vec<(PruningPriority, EntityAddress)> = self
            .due
            .iter()
            .filter(|(_, (prune_at, _))| *prune_at <= block)
            .map(|(key, (_, priority))| (*priority, *key))
            .collect();
        // Highest priority first; ties in ascending key order. Deterministic —
        // the purge turns this list into consensus deltas.
        due.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        Ok(due.into_iter().map(|(_, key)| key).collect())
    }

    fn clear_pruning(&mut self, entities: &[EntityAddress]) -> Result<(), Self::Error> {
        for key in entities {
            self.due.remove(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(byte: u8) -> EntityAddress {
        [byte; 32]
    }

    #[test]
    fn schedules_and_sweeps() {
        let mut map = MemPruningMap::new();
        map.schedule_pruning(key_of(3), 30, 0).unwrap();
        map.schedule_pruning(key_of(1), 10, 0).unwrap();
        map.schedule_pruning(key_of(2), 99, 0).unwrap();
        // A reschedule is an upsert — the entity moves, it doesn't duplicate.
        map.schedule_pruning(key_of(2), 20, 0).unwrap();

        assert_eq!(map.pruning_due(5).unwrap(), Vec::<EntityAddress>::new());
        // Equal priority: ascending key order, regardless of insertion order.
        assert_eq!(
            map.pruning_due(30).unwrap(),
            vec![key_of(1), key_of(2), key_of(3)]
        );

        map.clear_pruning(&[key_of(1), key_of(3)]).unwrap();
        assert_eq!(map.pruning_due(30).unwrap(), vec![key_of(2)]);
    }

    /// Higher priority goes to the top of the pruning stack; keys only break
    /// ties.
    #[test]
    fn higher_priority_prunes_first() {
        let mut map = MemPruningMap::new();
        map.schedule_pruning(key_of(1), 10, 0).unwrap();
        map.schedule_pruning(key_of(2), 10, 9).unwrap();
        map.schedule_pruning(key_of(3), 10, 5).unwrap();
        map.schedule_pruning(key_of(4), 10, 9).unwrap();

        assert_eq!(
            map.pruning_due(10).unwrap(),
            vec![key_of(2), key_of(4), key_of(3), key_of(1)]
        );

        // A reschedule can promote: key 1 jumps the queue.
        map.schedule_pruning(key_of(1), 10, 255).unwrap();
        assert_eq!(
            map.pruning_due(10).unwrap(),
            vec![key_of(1), key_of(2), key_of(4), key_of(3)]
        );

        // Not-yet-due entities never surface, whatever their priority.
        map.schedule_pruning(key_of(5), 99, 255).unwrap();
        assert_eq!(map.pruning_due(10).unwrap().len(), 4);
    }
}
