//! [`RethAuxStore`] — the query index over the entities.
//!
//! Sits on a backend that is both an
//! [`AccountCode`](crate::entities::AccountCode) (tier-1 pair bitmaps live in
//! account code) and an [`IndexStorage`] (tier-2 range structures, and the id→key
//! map, live in storage slots). The reth adapter fills both over revm state; the
//! store logic here is agnostic to how.
//!
//! Its jobs: [`apply_delta`](RethAuxStore::apply_delta) folds per-entity changes
//! in; [`evaluate`](RethAuxStore::evaluate) runs a whole [`Query`] (the RPC read
//! path); and the three primitives the spec's view exposes — `equal_entities`,
//! `prefixed_entities`, `entities_in_range` — answer one lookup each.
//!
//! ## Entity ids: allocation and the two maps
//!
//! Bitmaps are keyed on a compact, dense `u64` entity id, but a delta names an
//! entity by its [`EntityAddress`] and a query answers in keys — so the store owns the
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

use core::ops::Bound;
use std::collections::BTreeMap;

use crate::entities::AccountCode;
use crate::entities::layout::SYSTEM_ACCOUNT_ADDRESS;
use alloy_primitives::{B256, keccak256};
use arkiv_interfaces::entity::{AttributeType, AttributeValue};
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::query::{PageParams, Query, QueryMatches, QueryStats};

use crate::indices::address::pair_address;
use crate::indices::annotation::{QueryCapabilities, capabilities_for};
use crate::indices::bitmap::Bitmap;
use crate::indices::delta::AuxiliaryEntityDelta;
use crate::indices::equality_index::EqualityIndex;
use crate::indices::error::AuxError;
use crate::indices::range;
use crate::indices::range_index::RangeIndex;
use crate::indices::slot::{storage_to_u64, u64_to_storage};
use crate::indices::storage::IndexStorage;
use crate::indices::{index, interpret};

/// [`SYSTEM_ACCOUNT_ADDRESS`] slot holding the next entity id to allocate:
/// `keccak256("arkiv.entity_count")`. Domain-tagged to stay clear of the entity
/// store's own bookkeeping (`nonces`, …) on the shared account.
pub fn entity_count_slot() -> B256 {
    keccak256(b"arkiv.entity_count")
}

/// [`SYSTEM_ACCOUNT_ADDRESS`] slot mapping `key` to its entity id (stored as
/// `id + 1`): `keccak256("arkiv.key2id" || key)`.
pub fn key_to_id_slot(key: EntityAddress) -> B256 {
    const DOMAIN: &[u8] = b"arkiv.key2id";
    let mut buf = [0u8; DOMAIN.len() + size_of::<EntityAddress>()];
    buf[..DOMAIN.len()].copy_from_slice(DOMAIN);
    buf[DOMAIN.len()..].copy_from_slice(&key);
    keccak256(buf)
}

/// [`SYSTEM_ACCOUNT_ADDRESS`] slot mapping `entity_id` to its [`EntityAddress`]:
/// `keccak256("arkiv.id2key" || entity_id_be)`.
pub fn id_to_key_slot(entity_id: u64) -> B256 {
    const DOMAIN: &[u8] = b"arkiv.id2key";
    let mut buf = [0u8; DOMAIN.len() + size_of::<u64>()];
    buf[..DOMAIN.len()].copy_from_slice(DOMAIN);
    buf[DOMAIN.len()..].copy_from_slice(&entity_id.to_be_bytes());
    keccak256(buf)
}

/// The reth-host query index over a code + storage backend.
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
    fn id_for_key(&mut self, key: EntityAddress) -> Result<u64, AuxError<E>> {
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
    fn id_key(&mut self, entity_id: u64) -> Result<Option<EntityAddress>, AuxError<E>> {
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

impl<B, E> RethAuxStore<B>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    /// Run a whole [`Query`] to one page of matching keys, newest-first.
    pub fn evaluate(
        &mut self,
        query: &Query,
        page: PageParams,
    ) -> Result<QueryMatches, AuxError<E>> {
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

    pub fn apply_delta(&mut self, deltas: &[AuxiliaryEntityDelta]) -> Result<(), AuxError<E>> {
        for entity in deltas {
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

    /// Fold **insert-only** deltas in bulk: every `(attribute, value)` pair's
    /// bitmap is read and written once for the whole batch, instead of once per
    /// entity as [`apply_delta`](Self::apply_delta) does.
    ///
    /// For an insert-only batch the resulting *pair accounts* are byte-identical
    /// to the per-entity path: a bitmap's serialization depends only on the set
    /// of ids it holds, and a value enters the range index exactly once, when
    /// its bitmap first becomes non-empty. Only the range index's internal node
    /// layout may differ, because values are recorded in `(attribute, type,
    /// value)` order rather than in entity order. That makes this a tool for
    /// building a *fresh* state — genesis seeding — not a replacement for the
    /// per-block write path, whose layout is consensus.
    ///
    /// Entity ids are allocated in delta order, as `apply_delta` would. A delta
    /// carrying removes is rejected with [`AuxError::BulkRemovesUnsupported`].
    pub fn apply_inserts_bulk(
        &mut self,
        deltas: &[AuxiliaryEntityDelta],
    ) -> Result<(), AuxError<E>> {
        // (attr, typeId, index bytes) → ids, in a deterministic order. The type
        // id is part of the key because one attribute name may hold two types,
        // each in its own bucket.
        let mut groups: BTreeMap<(Vec<u8>, u8, Vec<u8>), Vec<u64>> = BTreeMap::new();
        for entity in deltas {
            if !entity.removes.is_empty() {
                return Err(AuxError::BulkRemovesUnsupported);
            }
            let entity_id = self.id_for_key(entity.entity_key)?;
            for entry in &entity.inserts {
                let ty = entry.value.attr_type();
                if capabilities_for(&entry.attr, ty) == QueryCapabilities::None {
                    continue;
                }
                groups
                    .entry((entry.attr.clone(), ty.id(), entry.value.index_bytes()))
                    .or_default()
                    .push(entity_id);
            }
        }
        for ((attr, type_id, bytes), ids) in groups {
            let ty = AttributeType::from_id(type_id).expect("came from a real attribute type");
            let addr = pair_address(&attr, ty, &bytes);
            let mut bitmap = EqualityIndex::new(&mut self.backend).bucket(addr)?;
            let was_empty = bitmap.is_empty();
            for id in ids {
                bitmap.insert(id);
            }
            self.backend
                .set_code(addr, bitmap.to_bytes())
                .map_err(AuxError::Backend)?;
            if was_empty {
                let capabilities = capabilities_for(&attr, ty);
                RangeIndex::new(&mut self.backend).insert(&attr, ty, &bytes, capabilities)?;
            }
        }
        Ok(())
    }

    /// Every live entity id — the `$all` bucket, read whole.
    pub fn all_entities(&mut self) -> Result<Bitmap, AuxError<E>> {
        EqualityIndex::new(&mut self.backend).all_entities()
    }

    /// The key behind an entity id, or `None` if the id was never allocated.
    pub fn key_of_id(&mut self, entity_id: u64) -> Result<Option<EntityAddress>, AuxError<E>> {
        self.id_key(entity_id)
    }

    /// Same type, same bytes. Ascending.
    pub fn equal_entities(
        &mut self,
        attr: &[u8],
        value: &AttributeValue,
    ) -> Result<Vec<EntityAddress>, AuxError<E>> {
        let hits = interpret::eq_bitmap(&mut self.backend, attr, value)?;
        self.keys_of(&hits)
    }

    /// Str-typed prefix match. Ascending.
    pub fn prefixed_entities(
        &mut self,
        attr: &[u8],
        prefix: &str,
    ) -> Result<Vec<EntityAddress>, AuxError<E>> {
        let value = AttributeValue::Str(prefix.into());
        let hits = interpret::prefix_bitmap(&mut self.backend, attr, &value)?;
        self.keys_of(&hits)
    }

    /// The bounds' type names the buckets scanned; a fully unbounded pair is
    /// [`AuxError::UnboundedRange`]. Ascending.
    pub fn entities_in_range(
        &mut self,
        attr: &[u8],
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
    ) -> Result<Vec<EntityAddress>, AuxError<E>> {
        let low_hits = match low {
            Bound::Included(v) => Some(interpret::range_bitmap(
                &mut self.backend,
                attr,
                v,
                range::Bound::Gte,
            )?),
            Bound::Excluded(v) => Some(interpret::range_bitmap(
                &mut self.backend,
                attr,
                v,
                range::Bound::Gt,
            )?),
            Bound::Unbounded => None,
        };
        let high_hits = match high {
            Bound::Included(v) => Some(interpret::range_bitmap(
                &mut self.backend,
                attr,
                v,
                range::Bound::Lte,
            )?),
            Bound::Excluded(v) => Some(interpret::range_bitmap(
                &mut self.backend,
                attr,
                v,
                range::Bound::Lt,
            )?),
            Bound::Unbounded => None,
        };
        // Mixed-type bounds intersect two disjoint typed bucket sets — empty.
        let hits = match (low_hits, high_hits) {
            (Some(mut l), Some(h)) => {
                l.intersect_with(&h);
                l
            }
            (Some(l), None) => l,
            (None, Some(h)) => h,
            (None, None) => return Err(AuxError::UnboundedRange),
        };
        self.keys_of(&hits)
    }

    fn keys_of(&mut self, hits: &Bitmap) -> Result<Vec<EntityAddress>, AuxError<E>> {
        let mut keys = Vec::with_capacity(hits.len() as usize);
        for entity_id in hits.iter() {
            if let Some(key) = self.id_key(entity_id)? {
                keys.push(key);
            }
        }
        keys.sort_unstable();
        Ok(keys)
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

    use crate::indices::delta::AttrEntry;

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

    fn key_of(byte: u8) -> EntityAddress {
        [byte; 32]
    }

    fn addr_of(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    struct NewEntity {
        key: EntityAddress,
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
        store.apply_delta(&deltas).unwrap();
    }

    /// Evaluate `query` and return the matching keys, sorted for stable assertions.
    fn matching(store: &mut RethAuxStore<MemBackend>, query: &Query) -> Vec<EntityAddress> {
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
            Vec::<EntityAddress>::new()
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
            Vec::<EntityAddress>::new()
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

    /// The bulk insert-only fold lands the same pair accounts and answers the
    /// same queries as the per-entity path — the property genesis seeding
    /// relies on.
    #[test]
    fn bulk_inserts_match_the_per_entity_path() {
        let deltas: Vec<_> = (0..40u8)
            .map(|i| {
                let mut attrs = user_uint("rank", u64::from(i % 5));
                attrs.extend(user_str("name", &format!("n{}", i % 3)));
                attrs.extend(user_int("delta", i32::from(i) - 20));
                create(&sample(i, i % 4, 100 + u64::from(i % 7), attrs))
            })
            .collect();

        let mut sequential = RethAuxStore::new(MemBackend::default());
        sequential.apply_delta(&deltas).unwrap();
        let mut bulk = RethAuxStore::new(MemBackend::default());
        bulk.apply_inserts_bulk(&deltas).unwrap();

        // Ids allocate identically, so every pair account holds the same bytes.
        assert_eq!(sequential.backend().code, bulk.backend().code);

        let queries = [
            Query::All,
            owner_is(addr_of(1)),
            Query::Gte {
                key: AnnotKey::User("rank".into()),
                value: word(3),
            },
            Query::Lt {
                key: AnnotKey::User("delta".into()),
                value: AnnotVal::Int(-10),
            },
            Query::StartsWith {
                key: AnnotKey::User("name".into()),
                value: AnnotVal::Str("n1".into()),
            },
            Query::Gt {
                key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
                value: word(103),
            },
        ];
        for query in &queries {
            assert_eq!(
                matching(&mut sequential, query),
                matching(&mut bulk, query),
                "{query:?}"
            );
        }
    }

    #[test]
    fn bulk_rejects_removes() {
        let mut store = RethAuxStore::new(MemBackend::default());
        let delete = AuxiliaryEntityDelta {
            entity_key: key_of(1),
            inserts: Vec::new(),
            removes: vec![entry(ALL, AttributeValue::Str(String::new()))],
        };
        assert!(matches!(
            store.apply_inserts_bulk(&[delete]),
            Err(AuxError::BulkRemovesUnsupported)
        ));
    }

    #[test]
    fn all_entities_and_key_of_id_round_trip() {
        let mut store = RethAuxStore::new(MemBackend::default());
        apply(
            &mut store,
            vec![
                create(&sample(0xA0, 1, 100, vec![])),
                create(&sample(0xB0, 2, 100, vec![])),
            ],
        );
        let all: Vec<u64> = store.all_entities().unwrap().iter().collect();
        assert_eq!(all, vec![0, 1]);
        assert_eq!(store.key_of_id(0).unwrap(), Some(key_of(0xA0)));
        assert_eq!(store.key_of_id(1).unwrap(), Some(key_of(0xB0)));
        assert_eq!(store.key_of_id(2).unwrap(), None);
    }

    /// A `u64` as a `u256` query bound.
    fn word(n: u64) -> AnnotVal {
        AnnotVal::u256_from_u64(n)
    }
}
