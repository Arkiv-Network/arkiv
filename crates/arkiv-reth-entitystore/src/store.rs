//! [`RethEntityStore`] — the [`EntityStore`] implementation.
//!
//! The store's own job is small and host-agnostic: map an [`EntityKey`] to its
//! account via [`entity_address`](crate::layout::entity_address), then load, store,
//! or remove the entity there. The *raw* persistence — how an entity is encoded
//! into an account's code and where those accounts actually live — is the
//! [`EntityBackend`] seam, which a reth adapter fills over revm's state (next
//! module). Splitting it this way keeps the trait logic testable without reth.

use alloy_primitives::Address;

use arkiv_interfaces::entity::Entity;
use arkiv_interfaces::primitives::{EntityKey, Hash};
use arkiv_interfaces::state::{BlockEntityStoreDelta, EntityStore};

use crate::layout::entity_address;

/// Raw account-level persistence the entity store sits on.
///
/// One entity per account address: `load`/`store`/`remove` it, and `root` gives a
/// commitment over the whole set. A reth adapter implements this over revm's
/// `Database`/state (encoding the entity into the account's code); the store logic
/// in [`RethEntityStore`] is agnostic to how that is done.
pub trait EntityBackend {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The entity stored at `addr`, or `None` if the account holds no entity.
    fn load(&mut self, addr: Address) -> Result<Option<Entity>, Self::Error>;

    /// Write `entity` to the account at `addr`.
    fn store(&mut self, addr: Address, entity: &Entity) -> Result<(), Self::Error>;

    /// Remove any entity at `addr`.
    fn remove(&mut self, addr: Address) -> Result<(), Self::Error>;

    /// Commitment over every stored entity.
    fn root(&mut self) -> Result<Hash, Self::Error>;
}

/// The reth-host [`EntityStore`]: entity keys resolve to accounts through
/// [`entity_address`](crate::layout::entity_address); persistence is delegated to
/// an [`EntityBackend`].
#[derive(Debug, Default, Clone)]
pub struct RethEntityStore<B> {
    backend: B,
}

impl<B> RethEntityStore<B> {
    /// Wrap a backend.
    pub const fn new(backend: B) -> Self {
        Self { backend }
    }

    /// The underlying backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Unwrap the backend.
    pub fn into_backend(self) -> B {
        self.backend
    }
}

impl<B: EntityBackend> EntityStore for RethEntityStore<B> {
    type Error = B::Error;

    fn get(&mut self, entity: EntityKey) -> Result<Option<Entity>, Self::Error> {
        self.backend.load(entity_address(entity))
    }

    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Self::Error> {
        for entity in &delta.puts {
            self.backend.store(entity_address(entity.key), entity)?;
        }
        for key in &delta.deletes {
            self.backend.remove(entity_address(*key))?;
        }
        Ok(())
    }

    fn commitment(&mut self) -> Result<Hash, Self::Error> {
        self.backend.root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use std::collections::HashMap;

    /// In-memory [`EntityBackend`] — an account→entity map. A stand-in for the
    /// reth-backed adapter; enough to exercise the store logic.
    #[derive(Default)]
    struct MemBackend {
        accounts: HashMap<Address, Entity>,
    }

    impl EntityBackend for MemBackend {
        type Error = Infallible;

        fn load(&mut self, addr: Address) -> Result<Option<Entity>, Infallible> {
            Ok(self.accounts.get(&addr).cloned())
        }
        fn store(&mut self, addr: Address, entity: &Entity) -> Result<(), Infallible> {
            self.accounts.insert(addr, entity.clone());
            Ok(())
        }
        fn remove(&mut self, addr: Address) -> Result<(), Infallible> {
            self.accounts.remove(&addr);
            Ok(())
        }
        fn root(&mut self) -> Result<Hash, Infallible> {
            Ok(Hash::default())
        }
    }

    fn entity_with_key(key: EntityKey) -> Entity {
        Entity {
            key,
            owner: [2u8; 20],
            expires_at: 100,
            payload: b"hi".to_vec(),
            ..Entity::default()
        }
    }

    #[test]
    fn put_get_delete_roundtrips_through_entity_address() {
        let mut store = RethEntityStore::new(MemBackend::default());
        let key: EntityKey = [7u8; 32];

        assert!(store.get(key).unwrap().is_none());

        let entity = entity_with_key(key);
        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: vec![entity.clone()],
                deletes: Vec::new(),
            })
            .unwrap();
        assert_eq!(store.get(key).unwrap().as_ref(), Some(&entity));

        // The entity really landed at its `entity_address`, not some other key.
        assert!(store.backend().accounts.contains_key(&entity_address(key)));

        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: Vec::new(),
                deletes: vec![key],
            })
            .unwrap();
        assert!(store.get(key).unwrap().is_none());
    }
}
