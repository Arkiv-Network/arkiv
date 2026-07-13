//! End-to-end index write path: run entity ops through [`ArkivExecutor::apply`],
//! feed the `draft.auxiliary` it stages into a real [`RethAuxStore`], and query the
//! entities back by their attributes.
//!
//! This is the join between module 5 (the executor building the index delta) and
//! module 4 (the auxiliary store applying it and answering queries). The executor's
//! delta and the store's encoding must agree exactly for a query to find anything,
//! so proving a create/transfer round-trips through both is the real test.

use std::collections::HashMap;
use std::convert::Infallible;

use alloy_primitives::{Address, B256};

use arkiv_interfaces::entity::{ATTR_UINT, Attribute};
use arkiv_interfaces::execution::{BlockDraft, ExecEnv, ExecStatus, Op};
use arkiv_interfaces::primitives::EntityKey;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, PageParams, Query};
use arkiv_interfaces::state::{AuxiliaryStore, BlockEntityStoreDelta, EntityStore};
use arkiv_reth_auxstore::IndexStorage;
use arkiv_reth_auxstore::RethAuxStore;
use arkiv_reth_entitystore::AccountCode;
use arkiv_reth_executor::ArkivExecutor;

// ── In-memory stores (stand-ins for the reth-backed ones) ──────────────────

/// A minimal [`EntityStore`] the executor reads through.
#[derive(Default)]
struct MemEntities {
    map: HashMap<EntityKey, arkiv_interfaces::entity::Entity>,
}

impl EntityStore for MemEntities {
    type Error = Infallible;
    fn get(
        &mut self,
        key: EntityKey,
    ) -> Result<Option<arkiv_interfaces::entity::Entity>, Infallible> {
        Ok(self.map.get(&key).cloned())
    }
    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Infallible> {
        for entity in &delta.puts {
            self.map.insert(entity.key, entity.clone());
        }
        for key in &delta.deletes {
            self.map.remove(key);
        }
        Ok(())
    }
    fn commitment(&mut self) -> Result<arkiv_interfaces::primitives::Hash, Infallible> {
        Ok(Default::default())
    }
}

/// A combined code + storage backend — the shape the reth bridge will have — for
/// the [`RethAuxStore`].
#[derive(Default)]
struct MemIndex {
    code: HashMap<Address, Vec<u8>>,
    slots: HashMap<(Address, B256), B256>,
}

impl AccountCode for MemIndex {
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

impl IndexStorage for MemIndex {
    type Error = Infallible;
    fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Infallible> {
        Ok(self.slots.get(&(addr, slot)).copied().unwrap_or(B256::ZERO))
    }
    fn set_storage(&mut self, addr: Address, slot: B256, value: B256) -> Result<(), Infallible> {
        self.slots.insert((addr, slot), value);
        Ok(())
    }
    fn ensure_account_persists(&mut self, _addr: Address) -> Result<(), Infallible> {
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn env(caller: [u8; 20], block: u64) -> ExecEnv {
    ExecEnv {
        caller,
        block_number: block,
        gas_supplied: 100_000_000,
        chain_id: 1,
    }
}

/// A `u64` as a 32-byte big-endian word — the canonical uint index encoding.
fn uint(n: u64) -> Vec<u8> {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    buf.to_vec()
}

fn word(n: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    buf
}

/// Run `ops` under `caller`/`block` and commit both the entity and index changes
/// into the stores.
fn run(
    exec: &ArkivExecutor<MemEntities>,
    entities: &mut MemEntities,
    index: &mut RethAuxStore<MemIndex>,
    caller: [u8; 20],
    block: u64,
    ops: &[Op],
) {
    let mut draft = BlockDraft::default();
    let out = exec
        .apply(&env(caller, block), entities, &mut draft, ops)
        .unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    index.apply_delta(&draft.auxiliary).unwrap();
    entities.apply_delta(&draft.entities).unwrap();
}

fn all(page: u64) -> PageParams {
    PageParams {
        page_size: page,
        cursor: None,
    }
}

fn keys(index: &mut RethAuxStore<MemIndex>, query: &Query) -> Vec<EntityKey> {
    let mut keys = index.evaluate(query, all(100)).unwrap().keys;
    keys.sort();
    keys
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// A create's staged index delta, applied to the store, makes the entity findable
/// by owner, by an exact user attribute, by a numeric range on that attribute, and
/// under `$all` — proving the executor's encoding matches the store's.
#[test]
fn create_is_queryable_by_its_attributes() {
    let exec = ArkivExecutor::<MemEntities>::new();
    let mut entities = MemEntities::default();
    let mut index = RethAuxStore::new(MemIndex::default());
    let alice = [0xAA; 20];
    let key: EntityKey = [1u8; 32];

    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 50,
            content_type: b"text/plain".to_vec(),
            payload: b"y".to_vec(),
            attributes: vec![Attribute {
                key: b"rank".to_vec(),
                value_type: ATTR_UINT,
                value: uint(42),
            }],
        }],
    );

    // By owner.
    assert_eq!(
        keys(
            &mut index,
            &Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Owner),
                value: AnnotVal::Addr(alice),
            }
        ),
        vec![key],
    );
    // By exact user attribute.
    assert_eq!(
        keys(
            &mut index,
            &Query::Eq {
                key: AnnotKey::User("rank".into()),
                value: AnnotVal::Uint(word(42)),
            }
        ),
        vec![key],
    );
    // By numeric range: rank >= 42 matches, rank > 42 does not.
    assert_eq!(
        keys(
            &mut index,
            &Query::Gte {
                key: AnnotKey::User("rank".into()),
                value: AnnotVal::Uint(word(42)),
            }
        ),
        vec![key],
    );
    assert!(
        keys(
            &mut index,
            &Query::Gt {
                key: AnnotKey::User("rank".into()),
                value: AnnotVal::Uint(word(42)),
            }
        )
        .is_empty()
    );
    // Under $all.
    assert_eq!(keys(&mut index, &Query::All), vec![key]);
}

/// After a transfer, the entity leaves the old owner's bucket and joins the new
/// one — the executor's remove/insert diff flowing through the store.
#[test]
fn transfer_moves_the_entity_between_owner_queries() {
    let exec = ArkivExecutor::<MemEntities>::new();
    let mut entities = MemEntities::default();
    let mut index = RethAuxStore::new(MemIndex::default());
    let alice = [0xAA; 20];
    let bob = [0xBB; 20];
    let key: EntityKey = [1u8; 32];

    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 50,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        11,
        &[Op::Transfer {
            key,
            new_owner: bob,
        }],
    );

    let owned_by = |owner: [u8; 20]| Query::Eq {
        key: AnnotKey::BuiltIn(BuiltIn::Owner),
        value: AnnotVal::Addr(owner),
    };
    assert!(keys(&mut index, &owned_by(alice)).is_empty());
    assert_eq!(keys(&mut index, &owned_by(bob)), vec![key]);
    // Still one live entity overall.
    assert_eq!(keys(&mut index, &Query::All), vec![key]);
}

/// A delete drops the entity from every query — its ids leave all buckets,
/// including `$all`.
#[test]
fn delete_removes_the_entity_from_queries() {
    let exec = ArkivExecutor::<MemEntities>::new();
    let mut entities = MemEntities::default();
    let mut index = RethAuxStore::new(MemIndex::default());
    let alice = [0xAA; 20];
    let key: EntityKey = [1u8; 32];

    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 50,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        11,
        &[Op::Delete { key }],
    );

    assert!(keys(&mut index, &Query::All).is_empty());
    assert!(
        keys(
            &mut index,
            &Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Owner),
                value: AnnotVal::Addr(alice),
            }
        )
        .is_empty()
    );
}

/// An update swaps the entity's old attribute value for the new one in the index:
/// the executor's before→after diff removes the old bucket entry and inserts the
/// new, so the old value stops matching and the new one starts.
#[test]
fn update_reindexes_attributes() {
    let exec = ArkivExecutor::<MemEntities>::new();
    let mut entities = MemEntities::default();
    let mut index = RethAuxStore::new(MemIndex::default());
    let alice = [0xAA; 20];
    let key: EntityKey = [1u8; 32];

    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 500,
            content_type: b"text/plain".to_vec(),
            payload: b"y".to_vec(),
            attributes: vec![Attribute {
                key: b"rank".to_vec(),
                value_type: ATTR_UINT,
                value: uint(10),
            }],
        }],
    );
    run(
        &exec,
        &mut entities,
        &mut index,
        alice,
        11,
        &[Op::Update {
            key,
            content_type: b"text/plain".to_vec(),
            payload: b"y".to_vec(),
            attributes: vec![Attribute {
                key: b"rank".to_vec(),
                value_type: ATTR_UINT,
                value: uint(20),
            }],
        }],
    );

    let rank_eq = |n: u64| Query::Eq {
        key: AnnotKey::User("rank".into()),
        value: AnnotVal::Uint(word(n)),
    };
    assert!(
        keys(&mut index, &rank_eq(10)).is_empty(),
        "old rank de-indexed"
    );
    assert_eq!(
        keys(&mut index, &rank_eq(20)),
        vec![key],
        "new rank indexed"
    );
}

/// The built-in `$expiration` field is range-indexed: entities are findable by a
/// numeric bound on their expiry block, exactly like a user uint attribute.
#[test]
fn expiration_is_range_queryable() {
    let exec = ArkivExecutor::<MemEntities>::new();
    let mut entities = MemEntities::default();
    let mut index = RethAuxStore::new(MemIndex::default());
    let alice = [0xAA; 20];

    let mut make = |key_byte: u8, expires_at: u64| {
        run(
            &exec,
            &mut entities,
            &mut index,
            alice,
            10,
            &[Op::Create {
                key: [key_byte; 32],
                expires_at,
                content_type: b"x".to_vec(),
                payload: b"y".to_vec(),
                attributes: Vec::new(),
            }],
        );
    };
    make(1, 50);
    make(2, 100);
    make(3, 150);

    // $expiration >= 100 matches the two later-expiring entities, not the first.
    let by_expiry = Query::Gte {
        key: AnnotKey::BuiltIn(BuiltIn::Expiration),
        value: AnnotVal::Uint(word(100)),
    };
    assert_eq!(keys(&mut index, &by_expiry), vec![[2u8; 32], [3u8; 32]]);
}
