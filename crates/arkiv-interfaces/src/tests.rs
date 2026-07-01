//! Tiny in-memory implementations that check the traits compile and can be
//! implemented with no dependencies. Not real behavior — this crate ships traits
//! only.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::convert::Infallible;

use crate::*;

/// A tiny in-memory [`EntityStore`] — a plain key→bytes map.
#[derive(Default)]
struct MemStore {
    entities: BTreeMap<EntityKey, Vec<u8>>,
}

impl EntityStore for MemStore {
    type Error = Infallible;

    fn get(&mut self, entity: EntityKey) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.entities.get(&entity).cloned())
    }
    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Self::Error> {
        for (key, bytes) in &delta.puts {
            self.entities.insert(*key, bytes.clone());
        }
        for key in &delta.deletes {
            self.entities.remove(key);
        }
        Ok(())
    }
    fn commitment(&mut self) -> Result<Hash, Self::Error> {
        Ok(Hash::default())
    }
}

#[test]
fn entity_store_roundtrips() {
    let mut s = MemStore::default();
    let e: EntityKey = [1u8; 32];
    assert!(s.get(e).unwrap().is_none());
    s.apply_delta(&BlockEntityStoreDelta {
        puts: alloc::vec![(e, alloc::vec![1, 2, 3])],
        deletes: Vec::new(),
    })
    .unwrap();
    assert_eq!(s.get(e).unwrap().unwrap(), alloc::vec![1, 2, 3]);
    s.apply_delta(&BlockEntityStoreDelta {
        puts: Vec::new(),
        deletes: alloc::vec![e],
    })
    .unwrap();
    assert!(s.get(e).unwrap().is_none());
    assert_eq!(s.commitment().unwrap(), Hash::default());
}

/// Touches the query/index types so they're exercised at compile time.
#[allow(dead_code)]
fn _touch_index_types(a: &mut dyn AuxiliaryStore<Error = Infallible>) {
    let _ = a.evaluate(
        &Query::All,
        PageParams {
            page_size: 10,
            cursor: None,
        },
    );
    let _ = a.apply_delta(&BlockAuxiliaryStoreDelta::default());
}
