//! [`CodeBackend`] — an [`EntityBackend`] over any [`AccountCode`].
//!
//! Each entity is stored as its record bytes ([`record::encode`]) in the account
//! `code` at its address; reads decode them back ([`record::decode`]). This is the
//! whole of entity persistence — everything reth-specific is behind the
//! [`AccountCode`] seam, so this logic is tested against an in-memory mock.

use alloy_primitives::Address;

use arkiv_interfaces::entity::Entity;
use arkiv_interfaces::primitives::Hash;

use crate::entities::account::AccountCode;
use crate::entities::record::{self, RecordError};
use crate::entities::store::EntityBackend;

/// An [`EntityBackend`] that keeps each entity in its account's `code`, over any
/// [`AccountCode`].
#[derive(Debug, Default, Clone)]
pub struct CodeBackend<C> {
    code: C,
}

impl<C> CodeBackend<C> {
    /// Wrap an [`AccountCode`].
    pub const fn new(code: C) -> Self {
        Self { code }
    }

    /// The underlying account-code store.
    pub fn account_code(&self) -> &C {
        &self.code
    }

    /// Unwrap the account-code store.
    pub fn into_inner(self) -> C {
        self.code
    }
}

impl<C: AccountCode> EntityBackend for CodeBackend<C> {
    type Error = CodeBackendError<C::Error>;

    fn load(&mut self, addr: Address) -> Result<Option<Entity>, Self::Error> {
        let code = self.code.code(addr).map_err(CodeBackendError::Code)?;
        if code.is_empty() {
            return Ok(None);
        }
        record::decode(&code)
            .map(Some)
            .map_err(CodeBackendError::Decode)
    }

    fn store(&mut self, addr: Address, entity: &Entity) -> Result<(), Self::Error> {
        self.code
            .set_code(addr, record::encode(entity))
            .map_err(CodeBackendError::Code)
    }

    fn remove(&mut self, addr: Address) -> Result<(), Self::Error> {
        self.code.clear_code(addr).map_err(CodeBackendError::Code)
    }

    fn root(&mut self) -> Result<Hash, Self::Error> {
        // The entity commitment is reth's account state root, which the host
        // produces at block commit (write path) or from a snapshot (read path) —
        // not from the code seam. Wired in a later module.
        Err(CodeBackendError::RootUnavailable)
    }
}

/// What can go wrong in a [`CodeBackend`].
#[derive(Debug)]
pub enum CodeBackendError<E> {
    /// The underlying [`AccountCode`] failed.
    Code(E),
    /// Stored code wasn't a valid entity record.
    Decode(RecordError),
    /// A commitment was requested but isn't available from the code seam.
    RootUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::store::RethEntityStore;
    use arkiv_interfaces::entity::CreationFlags;
    use arkiv_interfaces::entity::{Attribute, AttributeValue};
    use arkiv_interfaces::primitives::EntityAddress;
    use arkiv_interfaces::state::{BlockEntityStoreDelta, EntityStore};
    use core::convert::Infallible;
    use std::collections::HashMap;

    /// In-memory [`AccountCode`] — an address→code map.
    #[derive(Default)]
    struct MemCode {
        codes: HashMap<Address, Vec<u8>>,
    }

    impl AccountCode for MemCode {
        type Error = Infallible;

        fn code(&mut self, addr: Address) -> Result<Vec<u8>, Infallible> {
            Ok(self.codes.get(&addr).cloned().unwrap_or_default())
        }
        fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Infallible> {
            self.codes.insert(addr, code);
            Ok(())
        }
        fn clear_code(&mut self, addr: Address) -> Result<(), Infallible> {
            self.codes.remove(&addr);
            Ok(())
        }
    }

    fn sample() -> Entity {
        Entity {
            key: [7u8; 32],
            owner: [2u8; 20],
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"hi".to_vec(),
            attributes: vec![Attribute::new(
                b"c".to_vec(),
                AttributeValue::Str("blue".into()),
            )],
            ..Entity::default()
        }
    }

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    #[test]
    fn store_load_remove_roundtrips() {
        let mut backend = CodeBackend::new(MemCode::default());
        let e = sample();

        assert!(backend.load(addr(1)).unwrap().is_none());
        backend.store(addr(1), &e).unwrap();
        assert_eq!(backend.load(addr(1)).unwrap(), Some(e));
        backend.remove(addr(1)).unwrap();
        assert!(backend.load(addr(1)).unwrap().is_none());
    }

    #[test]
    fn load_surfaces_a_bad_record() {
        let mut backend = CodeBackend::new(MemCode::default());
        // Non-entity code at the address (no 0xFE prefix) is a decode error.
        backend.code.codes.insert(addr(9), vec![0x00, 0x11, 0x22]);
        assert!(matches!(
            backend.load(addr(9)),
            Err(CodeBackendError::Decode(_))
        ));
    }

    #[test]
    fn root_is_not_available_from_the_code_seam() {
        let mut backend = CodeBackend::new(MemCode::default());
        assert!(matches!(
            backend.root(),
            Err(CodeBackendError::RootUnavailable)
        ));
    }

    #[test]
    fn drives_a_reth_entity_store_end_to_end() {
        // RethEntityStore → CodeBackend → MemCode: apply a delta, read it back by
        // key (which resolves to the entity address internally).
        let mut store = RethEntityStore::new(CodeBackend::new(MemCode::default()));
        let key: EntityAddress = [7u8; 32];
        let e = sample();

        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: vec![e.clone()],
                deletes: Vec::new(),
            })
            .unwrap();
        assert_eq!(store.get(key).unwrap(), Some(e));

        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: Vec::new(),
                deletes: vec![key],
            })
            .unwrap();
        assert!(store.get(key).unwrap().is_none());
    }
}
