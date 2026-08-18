//! [`RethAuxStore`] — the [`AuxiliaryStore`] implementation.
//!
//! Sits on a backend that is both an
//! [`AccountCode`](arkiv_reth_entitystore::AccountCode) (tier-1 pair bitmaps live in
//! account code) and an [`IndexStorage`] (tier-2 range structures, and the id→key
//! map, live in storage slots). The reth adapter fills both over revm state; the
//! store logic here is agnostic to how.
//!
//! Its three jobs:
//! - [`apply_delta`](RethAuxStore::apply_delta) folds a block's per-entity changes
//!   into the index (via [`index`](crate::index)), resolving each entity's key to
//!   its id first.
//! - [`evaluate`](RethAuxStore::evaluate) runs a query (via
//!   [`interpret`](crate::interpret)) to a set of ids, pages them newest-first, and
//!   maps the page back to entity keys.
//! - [`commitment`](RethAuxStore::commitment) — see the note on the method.
//!
//! ## Entity ids: allocation and the two maps
//!
//! Bitmaps are keyed on a compact, dense `u64` entity id, but a delta names an
//! entity by its [`EntityKey`] and a query answers in keys — so the store owns the
//! whole id bookkeeping, all at [`SYSTEM_ACCOUNT_ADDRESS`]:
//! - a **counter** (next id to hand out),
//! - **key → id**, so a later op on an existing entity reuses its id, and
//! - **id → key**, so a query result's ids become keys.
//!
//! [`id_for_key`](RethAuxStore::id_for_key) allocates on a key's first appearance
//! (bumping the counter, writing both maps) and looks up on every appearance after —
//! ids are therefore dense and monotonic in creation order, deterministic across
//! nodes given the same block. A deleted entity leaves every bitmap (including
//! `$all`), so evaluation never surfaces its id; its `id → key` entry is harmless and
//! left in place. Both maps store `id + 1`, so an unwritten slot (`0`) reads as
//! "absent" without colliding with the genuine id `0`.

use alloy_primitives::{B256, keccak256};
use arkiv_interfaces::primitives::{EntityKey, Hash};
use arkiv_interfaces::query::{PageParams, Query, QueryMatches, QueryStats};
use arkiv_interfaces::state::{AuxiliaryStore, BlockAuxiliaryStoreDelta};
use arkiv_reth_entitystore::AccountCode;
use arkiv_reth_entitystore::layout::SYSTEM_ACCOUNT_ADDRESS;

use crate::annotation::capabilities_for;
use crate::error::AuxError;
use crate::slot::{storage_to_u64, u64_to_storage};
use crate::storage::IndexStorage;
use crate::{index, interpret};

/// [`SYSTEM_ACCOUNT_ADDRESS`] slot holding the next entity id to allocate:
/// `keccak256("arkiv.entity_count")`. Domain-tagged to stay clear of the entity
/// store's own bookkeeping (`nonces`, …) on the shared account.
fn entity_count_slot() -> B256 {
    keccak256(b"arkiv.entity_count")
}

/// [`SYSTEM_ACCOUNT_ADDRESS`] slot mapping `key` to its entity id (stored as
/// `id + 1`): `keccak256("arkiv.key2id" || key)`.
fn key_to_id_slot(key: EntityKey) -> B256 {
    const DOMAIN: &[u8] = b"arkiv.key2id";
    let mut buf = [0u8; DOMAIN.len() + size_of::<EntityKey>()];
    buf[..DOMAIN.len()].copy_from_slice(DOMAIN);
    buf[DOMAIN.len()..].copy_from_slice(&key);
    keccak256(buf)
}

/// [`SYSTEM_ACCOUNT_ADDRESS`] slot mapping `entity_id` to its [`EntityKey`]:
/// `keccak256("arkiv.id2key" || entity_id_be)`.
fn id_to_key_slot(entity_id: u64) -> B256 {
    const DOMAIN: &[u8] = b"arkiv.id2key";
    let mut buf = [0u8; DOMAIN.len() + size_of::<u64>()];
    buf[..DOMAIN.len()].copy_from_slice(DOMAIN);
    buf[DOMAIN.len()..].copy_from_slice(&entity_id.to_be_bytes());
    keccak256(buf)
}

/// The reth-host [`AuxiliaryStore`]: the index over a code + storage backend.
#[derive(Debug, Default, Clone)]
pub struct RethAuxStore<B> {
    backend: B,
}

impl<B> RethAuxStore<B> {
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

impl<B, E> RethAuxStore<B>
where
    B: IndexStorage<Error = E>,
{
    /// The entity id for `key`: its existing id, or a freshly allocated one if this
    /// is the key's first appearance (bumping the counter and writing both maps).
    fn id_for_key(&mut self, key: EntityKey) -> Result<u64, AuxError<E>> {
        let key_slot = key_to_id_slot(key);
        let existing = self
            .backend
            .storage(SYSTEM_ACCOUNT_ADDRESS, key_slot)
            .map_err(AuxError::Backend)?;
        if existing != B256::ZERO {
            return Ok(storage_to_u64(existing) - 1); // stored as id + 1
        }

        // First sight of this key: allocate the next id and record both maps.
        self.backend
            .ensure_account_persists(SYSTEM_ACCOUNT_ADDRESS)
            .map_err(AuxError::Backend)?;
        let count_slot = entity_count_slot();
        let new_id = storage_to_u64(
            self.backend
                .storage(SYSTEM_ACCOUNT_ADDRESS, count_slot)
                .map_err(AuxError::Backend)?,
        );
        self.write_slot(count_slot, u64_to_storage(new_id + 1))?;
        self.write_slot(key_slot, u64_to_storage(new_id + 1))?;
        self.write_slot(id_to_key_slot(new_id), B256::from(key))?;
        Ok(new_id)
    }

    /// The key for `entity_id`, or `None` if the id was never allocated.
    fn id_key(&mut self, entity_id: u64) -> Result<Option<EntityKey>, AuxError<E>> {
        let word = self
            .backend
            .storage(SYSTEM_ACCOUNT_ADDRESS, id_to_key_slot(entity_id))
            .map_err(AuxError::Backend)?;
        if word == B256::ZERO {
            Ok(None)
        } else {
            Ok(Some(word.0))
        }
    }

    /// Write a bookkeeping slot on the system account.
    fn write_slot(&mut self, slot: B256, value: B256) -> Result<(), AuxError<E>> {
        self.backend
            .set_storage(SYSTEM_ACCOUNT_ADDRESS, slot, value)
            .map_err(AuxError::Backend)
    }
}

impl<B, E> AuxiliaryStore for RethAuxStore<B>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    type Error = AuxError<E>;

    fn evaluate(&mut self, query: &Query, page: PageParams) -> Result<QueryMatches, Self::Error> {
        let matches = interpret::eval(query, &mut self.backend)?;

        // Roaring iterates ascending; page the largest ids first (newest entities),
        // with `cursor` an exclusive upper bound continuing a previous page downward.
        let mut ids: Vec<u64> = matches.iter().collect();
        if let Some(cursor) = page.cursor {
            ids.retain(|id| *id < cursor);
        }
        let page_size = page.page_size as usize;
        let page_ids: Vec<u64> = ids.iter().rev().take(page_size).copied().collect();
        let next_cursor = (ids.len() > page_ids.len())
            .then(|| page_ids.last().copied())
            .flatten();

        let mut keys = Vec::with_capacity(page_ids.len());
        for entity_id in &page_ids {
            if let Some(key) = self.id_key(*entity_id)? {
                keys.push(key);
            }
        }

        let stats = QueryStats {
            entities_scanned: matches.len(),
            entities_returned: keys.len() as u64,
            index_lookups: page_ids.len() as u64,
            gas_used: 0,
            partial: false,
        };
        Ok(QueryMatches {
            keys,
            next_cursor,
            stats,
        })
    }

    fn apply_delta(&mut self, delta: &BlockAuxiliaryStoreDelta) -> Result<(), Self::Error> {
        for entity in &delta.entities {
            let entity_id = self.id_for_key(entity.entity_key)?;
            for entry in &entity.inserts {
                let capabilities = capabilities_for(&entry.attr, entry.value.attr_type());
                index::insert(
                    &mut self.backend,
                    &entry.attr,
                    &entry.value,
                    entity_id,
                    capabilities,
                )?;
            }
            for entry in &entity.removes {
                let capabilities = capabilities_for(&entry.attr, entry.value.attr_type());
                index::remove(
                    &mut self.backend,
                    &entry.attr,
                    &entry.value,
                    entity_id,
                    capabilities,
                )?;
            }
        }
        Ok(())
    }

    fn commitment(&mut self) -> Result<Hash, Self::Error> {
        // On the reth host the index commits through the block's unified state root:
        // every pair, tier-2, and bookkeeping account lives in the one state trie,
        // alongside the entities. A store-scoped sub-commitment would need its own
        // sub-trie, which the host does not maintain, so this returns the default.
        Ok(Hash::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use std::collections::HashMap;

    use alloy_primitives::Address;
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::entity::annotations::{
        ALL, CONTENT_TYPE, CREATED_AT_BLOCK, CREATOR, EXPIRATION, KEY, OWNER,
    };
    use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn};
    use arkiv_interfaces::state::{AttrEntry, AuxiliaryEntityDelta};

    /// A backend that is both an [`AccountCode`] and an [`IndexStorage`] — the shape
    /// the reth bridge has. In-memory maps; enough to exercise the whole index.
    #[derive(Default)]
    struct MemBackend {
        code: HashMap<Address, Vec<u8>>,
        slots: HashMap<(Address, B256), B256>,
    }

    impl AccountCode for MemBackend {
        type Error = Infallible;
        fn code(&mut self, addr: Address) -> Result<Vec<u8>, Infallible> {
            Ok(self.code.get(&addr).cloned().unwrap_or_default())
        }
        fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Infallible> {
            self.code.insert(addr, code);
            Ok(())
        }
        fn clear_code(&mut self, addr: Address) -> Result<(), Infallible> {
            self.code.remove(&addr);
            Ok(())
        }
    }

    impl IndexStorage for MemBackend {
        type Error = Infallible;
        fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Infallible> {
            Ok(self.slots.get(&(addr, slot)).copied().unwrap_or(B256::ZERO))
        }
        fn set_storage(
            &mut self,
            addr: Address,
            slot: B256,
            value: B256,
        ) -> Result<(), Infallible> {
            self.slots.insert((addr, slot), value);
            Ok(())
        }
        fn ensure_account_persists(&mut self, _addr: Address) -> Result<(), Infallible> {
            Ok(())
        }
    }

    // ── delta builders (a stand-in for what the executor will produce) ────────

    fn entry(attr: &[u8], value: AttributeValue) -> AttrEntry {
        AttrEntry::new(attr, value)
    }

    fn key_of(byte: u8) -> EntityKey {
        [byte; 32]
    }

    fn addr_of(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    struct NewEntity {
        key: EntityKey,
        owner: [u8; 20],
        expires: u64,
        content_type: String,
        attributes: Vec<AttrEntry>,
    }

    /// The full insert set for a created entity: the built-ins plus its user
    /// attributes — mirroring `arkiv-db-engine`'s `built_in_annotations`. The store
    /// allocates the entity id itself on first sight of the key.
    fn create(entity: &NewEntity) -> AuxiliaryEntityDelta {
        let mut inserts = vec![
            entry(ALL, AttributeValue::Str(String::new())),
            entry(CREATOR, AttributeValue::EthereumAddress(entity.owner)),
            entry(OWNER, AttributeValue::EthereumAddress(entity.owner)),
            entry(KEY, AttributeValue::EntityKey(entity.key)),
            entry(CREATED_AT_BLOCK, AttributeValue::u256_from_u64(1)),
            entry(EXPIRATION, AttributeValue::u256_from_u64(entity.expires)),
            entry(
                CONTENT_TYPE,
                AttributeValue::Str(entity.content_type.clone()),
            ),
        ];
        inserts.extend(entity.attributes.iter().cloned());
        AuxiliaryEntityDelta {
            entity_key: entity.key,
            inserts,
            removes: Vec::new(),
        }
    }

    fn apply(store: &mut RethAuxStore<MemBackend>, deltas: Vec<AuxiliaryEntityDelta>) {
        store
            .apply_delta(&BlockAuxiliaryStoreDelta { entities: deltas })
            .unwrap();
    }

    /// Evaluate `query` and return the matching keys, sorted for stable assertions.
    fn matching(store: &mut RethAuxStore<MemBackend>, query: &Query) -> Vec<EntityKey> {
        let page = PageParams {
            page_size: 1000,
            cursor: None,
        };
        let mut keys = store.evaluate(query, page).unwrap().keys;
        keys.sort();
        keys
    }

    fn owner_is(owner: [u8; 20]) -> Query {
        Query::Eq {
            key: AnnotKey::BuiltIn(BuiltIn::Owner),
            value: AnnotVal::EthereumAddress(owner),
        }
    }

    fn user_uint(name: &str, value: u64) -> Vec<AttrEntry> {
        vec![entry(name.as_bytes(), AttributeValue::u256_from_u64(value))]
    }

    fn user_str(name: &str, value: &str) -> Vec<AttrEntry> {
        vec![entry(name.as_bytes(), AttributeValue::Str(value.into()))]
    }

    fn user_int(name: &str, value: i32) -> Vec<AttrEntry> {
        vec![entry(name.as_bytes(), AttributeValue::Int(value))]
    }

    fn sample(key: u8, owner: u8, expires: u64, attributes: Vec<AttrEntry>) -> NewEntity {
        NewEntity {
            key: key_of(key),
            owner: addr_of(owner),
            expires,
            content_type: "text/plain".into(),
            attributes,
        }
    }

    #[test]
    fn equality_on_owner_and_the_id_to_key_map() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, vec![])),
                create(&sample(0xB0, 2, 100, vec![])),
                create(&sample(0xC0, 1, 100, vec![])),
            ],
        );

        // Two entities are owned by addr 1; the query answers in their full keys.
        assert_eq!(
            matching(&mut store, &owner_is(addr_of(1))),
            vec![key_of(0xA0), key_of(0xC0)],
        );
        assert_eq!(
            matching(&mut store, &owner_is(addr_of(2))),
            vec![key_of(0xB0)]
        );
        assert_eq!(
            matching(&mut store, &owner_is(addr_of(9))),
            Vec::<EntityKey>::new()
        );
    }

    #[test]
    fn all_matches_every_live_entity() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, vec![])),
                create(&sample(0xB0, 2, 100, vec![])),
            ],
        );
        assert_eq!(
            matching(&mut store, &Query::All),
            vec![key_of(0xA0), key_of(0xB0)],
        );
    }

    /// `NOT` is the language's only negation, and it is the full complement:
    /// everything live except the match, including entities that never carried
    /// the attribute at all.
    #[test]
    fn not_is_all_minus_the_match() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, vec![])),
                create(&sample(0xB0, 2, 100, vec![])),
                create(&sample(0xC0, 1, 100, vec![])),
            ],
        );
        let q = Query::Not(Box::new(owner_is(addr_of(1))));
        assert_eq!(matching(&mut store, &q), vec![key_of(0xB0)]);
    }

    /// The language dropped `IN`; an `OR` chain is the same query and costs the
    /// same — a union of the two values' bitmaps.
    #[test]
    fn or_unions_the_values() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, vec![])),
                create(&sample(0xB0, 2, 100, vec![])),
                create(&sample(0xC0, 3, 100, vec![])),
            ],
        );
        let q = Query::Or(
            Box::new(owner_is(addr_of(1))),
            Box::new(owner_is(addr_of(3))),
        );
        assert_eq!(matching(&mut store, &q), vec![key_of(0xA0), key_of(0xC0)]);
    }

    #[test]
    fn int_range_on_expiration() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 10, vec![])),
                create(&sample(0xB0, 1, 20, vec![])),
                create(&sample(0xC0, 1, 30, vec![])),
            ],
        );
        let gt = |n: u64| Query::Gt {
            key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
            value: word(n),
        };
        assert_eq!(matching(&mut store, &gt(20)), vec![key_of(0xC0)]);
        let lte = Query::Lte {
            key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
            value: word(20),
        };
        assert_eq!(matching(&mut store, &lte), vec![key_of(0xA0), key_of(0xB0)]);
    }

    #[test]
    fn int_range_on_a_user_uint_attribute() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, user_uint("rank", 5))),
                create(&sample(0xB0, 1, 100, user_uint("rank", 15))),
                create(&sample(0xC0, 1, 100, user_uint("rank", 25))),
            ],
        );
        let q = Query::Gte {
            key: AnnotKey::User("rank".into()),
            value: word(15),
        };
        assert_eq!(matching(&mut store, &q), vec![key_of(0xB0), key_of(0xC0)]);
    }

    /// Signed values are range-scanned in numeric order, negatives included —
    /// what the biased index encoding buys.
    #[test]
    fn int_range_spans_negatives() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, user_int("delta", -5))),
                create(&sample(0xB0, 1, 100, user_int("delta", 0))),
                create(&sample(0xC0, 1, 100, user_int("delta", 5))),
            ],
        );
        let gte = |n: i32| Query::Gte {
            key: AnnotKey::User("delta".into()),
            value: AnnotVal::Int(n),
        };
        assert_eq!(
            matching(&mut store, &gte(-5)),
            vec![key_of(0xA0), key_of(0xB0), key_of(0xC0)]
        );
        assert_eq!(
            matching(&mut store, &gte(0)),
            vec![key_of(0xB0), key_of(0xC0)]
        );
        let lt_zero = Query::Lt {
            key: AnnotKey::User("delta".into()),
            value: AnnotVal::Int(0),
        };
        assert_eq!(matching(&mut store, &lt_zero), vec![key_of(0xA0)]);
    }

    /// One attribute name holding two types keeps two disjoint buckets: a `bool`
    /// `true` and a one-byte string with the same byte don't answer each other.
    #[test]
    fn same_name_different_types_do_not_collide() {
        let mut store = RethAuxStore::new(MemBackend::default());
        let flag = |v: AttributeValue| vec![entry(b"flag", v)];
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, flag(AttributeValue::Bool(true)))),
                create(&sample(
                    0xB0,
                    1,
                    100,
                    flag(AttributeValue::Str("\u{1}".into())),
                )),
                create(&sample(
                    0xC0,
                    1,
                    100,
                    flag(AttributeValue::u256_from_u64(1)),
                )),
            ],
        );
        let eq = |v: AttributeValue| Query::Eq {
            key: AnnotKey::User("flag".into()),
            value: v,
        };
        assert_eq!(
            matching(&mut store, &eq(AttributeValue::Bool(true))),
            vec![key_of(0xA0)]
        );
        assert_eq!(
            matching(&mut store, &eq(AttributeValue::Str("\u{1}".into()))),
            vec![key_of(0xB0)]
        );
        assert_eq!(
            matching(&mut store, &eq(AttributeValue::u256_from_u64(1))),
            vec![key_of(0xC0)]
        );
    }

    /// `STARTSWITH` walks the tier-2 string cascade for the matching values and
    /// unions their bitmaps. Strings are prefix-indexed, not range-indexed — the
    /// language has no `<`/`>` for them.
    #[test]
    fn startswith_on_a_user_string_attribute() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, user_str("name", "apple"))),
                create(&sample(0xB0, 1, 100, user_str("name", "banana"))),
                create(&sample(0xC0, 1, 100, user_str("name", "blueberry"))),
            ],
        );
        let starts_with = |prefix: &str| Query::StartsWith {
            key: AnnotKey::User("name".into()),
            value: AnnotVal::Str(prefix.into()),
        };
        assert_eq!(
            matching(&mut store, &starts_with("b")),
            vec![key_of(0xB0), key_of(0xC0)]
        );
        assert_eq!(
            matching(&mut store, &starts_with("blue")),
            vec![key_of(0xC0)]
        );
        // An empty prefix matches every value of that attribute.
        assert_eq!(
            matching(&mut store, &starts_with("")),
            vec![key_of(0xA0), key_of(0xB0), key_of(0xC0)]
        );
    }

    #[test]
    fn and_or_not_compose() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 10, vec![])),
                create(&sample(0xB0, 1, 30, vec![])),
                create(&sample(0xC0, 2, 30, vec![])),
            ],
        );
        // owner==1 AND expiration>20  → only B0
        let and = Query::And(
            Box::new(owner_is(addr_of(1))),
            Box::new(Query::Gt {
                key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
                value: word(20),
            }),
        );
        assert_eq!(matching(&mut store, &and), vec![key_of(0xB0)]);

        // owner==2 OR expiration<20 → A0, C0
        let or = Query::Or(
            Box::new(owner_is(addr_of(2))),
            Box::new(Query::Lt {
                key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
                value: word(20),
            }),
        );
        assert_eq!(matching(&mut store, &or), vec![key_of(0xA0), key_of(0xC0)]);

        // NOT(owner==1) → C0
        let not = Query::Not(Box::new(owner_is(addr_of(1))));
        assert_eq!(matching(&mut store, &not), vec![key_of(0xC0)]);
    }

    #[test]
    fn delete_drops_an_entity_from_every_query() {
        let mut store = RethAuxStore::new(MemBackend::default());
        let a = sample(0xA0, 1, 100, user_uint("rank", 5));
        let b = sample(0xB0, 1, 100, user_uint("rank", 5));
        apply(&mut store, vec![create(&a), create(&b)]);

        // A delete removes every annotation the create inserted.
        let created = create(&a);
        let delete = AuxiliaryEntityDelta {
            entity_key: a.key,
            inserts: Vec::new(),
            removes: created.inserts,
        };
        apply(&mut store, vec![delete]);

        assert_eq!(matching(&mut store, &Query::All), vec![key_of(0xB0)]);
        assert_eq!(
            matching(&mut store, &owner_is(addr_of(1))),
            vec![key_of(0xB0)]
        );
        // The shared user value's bitmap still holds B0, so the value survives.
        let q = Query::Eq {
            key: AnnotKey::User("rank".into()),
            value: word(5),
        };
        assert_eq!(matching(&mut store, &q), vec![key_of(0xB0)]);
    }

    #[test]
    fn paging_returns_newest_first_and_a_resumable_cursor() {
        let mut store = RethAuxStore::new(MemBackend::default());
        let deltas: Vec<_> = (0..5)
            .map(|i| create(&sample(0xA0 + i as u8, 1, 100, vec![])))
            .collect();
        apply(&mut store, deltas);

        let first = store
            .evaluate(
                &Query::All,
                PageParams {
                    page_size: 2,
                    cursor: None,
                },
            )
            .unwrap();
        // Newest (largest id) first: ids 4, 3 → keys 0xA4, 0xA3.
        assert_eq!(first.keys, vec![key_of(0xA4), key_of(0xA3)]);
        assert_eq!(first.next_cursor, Some(3));

        let second = store
            .evaluate(
                &Query::All,
                PageParams {
                    page_size: 2,
                    cursor: first.next_cursor,
                },
            )
            .unwrap();
        assert_eq!(second.keys, vec![key_of(0xA2), key_of(0xA1)]);
        assert_eq!(second.next_cursor, Some(1));

        let third = store
            .evaluate(
                &Query::All,
                PageParams {
                    page_size: 2,
                    cursor: second.next_cursor,
                },
            )
            .unwrap();
        assert_eq!(third.keys, vec![key_of(0xA0)]);
        assert_eq!(third.next_cursor, None);
    }

    #[test]
    fn transfer_moves_an_entity_between_owner_buckets() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(&mut store, vec![create(&sample(0xA0, 1, 100, vec![]))]);

        // A transfer removes the old $owner value and inserts the new one.
        let transfer = AuxiliaryEntityDelta {
            entity_key: key_of(0xA0),
            inserts: vec![entry(OWNER, AttributeValue::EthereumAddress(addr_of(2)))],
            removes: vec![entry(OWNER, AttributeValue::EthereumAddress(addr_of(1)))],
        };
        apply(&mut store, vec![transfer]);

        assert_eq!(
            matching(&mut store, &owner_is(addr_of(1))),
            Vec::<EntityKey>::new()
        );
        assert_eq!(
            matching(&mut store, &owner_is(addr_of(2))),
            vec![key_of(0xA0)]
        );
    }

    #[test]
    fn id_for_key_allocates_densely_then_reuses() {
        let mut store = RethAuxStore::new(MemBackend::default());
        // Fresh keys get dense, monotonic ids.
        assert_eq!(store.id_for_key(key_of(0xA0)).unwrap(), 0);
        assert_eq!(store.id_for_key(key_of(0xB0)).unwrap(), 1);
        assert_eq!(store.id_for_key(key_of(0xC0)).unwrap(), 2);
        // A seen key resolves back to its existing id (including id 0).
        assert_eq!(store.id_for_key(key_of(0xA0)).unwrap(), 0);
        assert_eq!(store.id_for_key(key_of(0xB0)).unwrap(), 1);
        // The reverse map round-trips.
        assert_eq!(store.id_key(0).unwrap(), Some(key_of(0xA0)));
        assert_eq!(store.id_key(2).unwrap(), Some(key_of(0xC0)));
        assert_eq!(store.id_key(9).unwrap(), None);
    }

    /// A `u64` as a `u256` query bound.
    fn word(n: u64) -> AnnotVal {
        AnnotVal::u256_from_u64(n)
    }
}
