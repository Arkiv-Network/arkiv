//! Tiny in-memory implementations that check the traits compile and can be
//! implemented with no dependencies. Not real behavior — this crate ships traits
//! only.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::convert::Infallible;

use crate::*;

/// A tiny in-memory [`EntityStore`] — a plain key→[`Entity`] map. A real store
/// would serialize the entities; this one just keeps them as values.
#[derive(Default)]
struct MemStore {
    entities: BTreeMap<EntityKey, Entity>,
}

impl EntityStore for MemStore {
    type Error = Infallible;

    fn get(&mut self, entity: EntityKey) -> Result<Option<Entity>, Self::Error> {
        Ok(self.entities.get(&entity).cloned())
    }
    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Self::Error> {
        for entity in &delta.puts {
            self.entities.insert(entity.key, entity.clone());
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
    let key: EntityKey = [1u8; 32];
    let entity = Entity {
        key,
        payload: alloc::vec![1, 2, 3],
        ..Entity::default()
    };
    assert!(s.get(key).unwrap().is_none());
    s.apply_delta(&BlockEntityStoreDelta {
        puts: alloc::vec![entity.clone()],
        deletes: Vec::new(),
    })
    .unwrap();
    assert_eq!(s.get(key).unwrap().unwrap(), entity);
    s.apply_delta(&BlockEntityStoreDelta {
        puts: Vec::new(),
        deletes: alloc::vec![key],
    })
    .unwrap();
    assert!(s.get(key).unwrap().is_none());
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
