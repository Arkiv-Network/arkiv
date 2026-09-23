//! End-to-end index write path: ops → deltas → index stores → commit → query
//! back through the view's index primitives, all over the one `ChainView`.

use core::ops::Bound;

use arkiv_authenticated_store::Store;
use arkiv_interfaces::entity::annotations::{ALL, EXPIRATION, OWNER};
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags};
use arkiv_interfaces::execution::{AttributeMutation, ExecEnv, ExecStatus, Op};
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::statemanager::{
    BlockRef, EntityStore, EqualityIndexStore, RangeIndexStore, ReadMode, StateView,
};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_reth_statemanager::chain::{ChainView, chain_manager};
use reth_ethereum::evm::revm::database_interface::EmptyDB;
type Mgr<'a> = ChainView<'a, EmptyDB>;

fn env(caller: [u8; 20], block: u64) -> ExecEnv {
    ExecEnv {
        caller,
        block_number: block,
        gas_supplied: 100_000_000,
        chain_id: 1,
    }
}

/// A `u64` as a `u256` attribute value.
fn uint(n: u64) -> AttributeValue {
    AttributeValue::u256_from_u64(n)
}

/// Run `ops`, fold the staged deltas into the index stores, commit the view.
fn run<'a>(
    exec: &ArkivExecutor<Mgr<'a>>,
    mgr: &mut Mgr<'a>,
    caller: [u8; 20],
    block: u64,
    ops: &[Op],
) {
    let out = exec.apply(&env(caller, block), mgr, ops).unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    let deltas = mgr.get_uncommitted_deltas().unwrap();
    EqualityIndexStore::apply_deltas(mgr, &deltas).unwrap();
    RangeIndexStore::apply_deltas(mgr, &deltas).unwrap();
    StateView::commit(mgr).unwrap();
}

fn owned_by(mgr: &Mgr, owner: [u8; 20]) -> Vec<EntityAddress> {
    mgr.get_equal_entities(
        OWNER,
        &AttributeValue::EthereumAddress(owner),
        ReadMode::ViewOnBase,
    )
    .unwrap()
}

/// Every live entity — the `$all` marker's bucket.
fn all_entities(mgr: &Mgr) -> Vec<EntityAddress> {
    mgr.get_equal_entities(
        ALL,
        &AttributeValue::Str(String::new()),
        ReadMode::ViewOnBase,
    )
    .unwrap()
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// A create's staged delta, folded into the index, makes the entity findable by
/// owner, by an exact user attribute, by a numeric range on it, and under
/// `$all` — proving the view's delta derivation matches the store's encoding.
#[test]
fn create_is_queryable_by_its_attributes() {
    let exec = ArkivExecutor::<Mgr>::new();
    let mut db = EmptyDB::default();
    let mut mgr = chain_manager(&mut db, BlockRef::new(9, [0; 32]), Store::default()).unwrap();
    let alice = [0xAA; 20];
    let key: EntityAddress = [1u8; 32];

    run(
        &exec,
        &mut mgr,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"y".to_vec(),
            attributes: vec![Attribute::new(b"rank".to_vec(), uint(42))],
        }],
    );

    assert_eq!(owned_by(&mgr, alice), vec![key]);
    assert_eq!(
        mgr.get_equal_entities(b"rank", &uint(42), ReadMode::ViewOnBase)
            .unwrap(),
        vec![key],
    );
    // By numeric range: rank >= 42 matches, rank > 42 does not.
    assert_eq!(
        mgr.get_within_range(
            b"rank",
            Bound::Included(&uint(42)),
            Bound::Unbounded,
            ReadMode::ViewOnBase,
        )
        .unwrap(),
        vec![key],
    );
    assert!(
        mgr.get_within_range(
            b"rank",
            Bound::Excluded(&uint(42)),
            Bound::Unbounded,
            ReadMode::ViewOnBase,
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(all_entities(&mgr), vec![key]);
}

/// After a transfer, the entity leaves the old owner's bucket and joins the new
/// one — the derived remove/insert diff flowing through the store.
#[test]
fn transfer_moves_the_entity_between_owner_queries() {
    let exec = ArkivExecutor::<Mgr>::new();
    let mut db = EmptyDB::default();
    let mut mgr = chain_manager(&mut db, BlockRef::new(9, [0; 32]), Store::default()).unwrap();
    let alice = [0xAA; 20];
    let bob = [0xBB; 20];
    let key: EntityAddress = [1u8; 32];

    run(
        &exec,
        &mut mgr,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(
        &exec,
        &mut mgr,
        alice,
        11,
        &[Op::Transfer {
            key,
            new_owner: bob,
        }],
    );

    assert!(owned_by(&mgr, alice).is_empty());
    assert_eq!(owned_by(&mgr, bob), vec![key]);
    // Still one live entity overall.
    assert_eq!(all_entities(&mgr), vec![key]);
}

/// A delete drops the entity from every query — its id leaves all buckets,
/// including `$all`.
#[test]
fn delete_removes_the_entity_from_queries() {
    let exec = ArkivExecutor::<Mgr>::new();
    let mut db = EmptyDB::default();
    let mut mgr = chain_manager(&mut db, BlockRef::new(9, [0; 32]), Store::default()).unwrap();
    let alice = [0xAA; 20];
    let key: EntityAddress = [1u8; 32];

    run(
        &exec,
        &mut mgr,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(&exec, &mut mgr, alice, 11, &[Op::Delete { key }]);

    assert!(all_entities(&mgr).is_empty());
    assert!(owned_by(&mgr, alice).is_empty());
}

/// An update swaps the entity's old attribute value for the new one in the
/// index: the old value stops matching and the new one starts.
#[test]
fn update_reindexes_attributes() {
    let exec = ArkivExecutor::<Mgr>::new();
    let mut db = EmptyDB::default();
    let mut mgr = chain_manager(&mut db, BlockRef::new(9, [0; 32]), Store::default()).unwrap();
    let alice = [0xAA; 20];
    let key: EntityAddress = [1u8; 32];

    run(
        &exec,
        &mut mgr,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 500,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"y".to_vec(),
            attributes: vec![Attribute::new(b"rank".to_vec(), uint(10))],
        }],
    );
    run(
        &exec,
        &mut mgr,
        alice,
        11,
        &[Op::Patch {
            key,
            mutations: vec![AttributeMutation::set(b"rank".to_vec(), uint(20))],
        }],
    );

    let rank_eq = |mgr: &Mgr, n: u64| {
        mgr.get_equal_entities(b"rank", &uint(n), ReadMode::ViewOnBase)
            .unwrap()
    };
    assert!(rank_eq(&mgr, 10).is_empty(), "old rank de-indexed");
    assert_eq!(rank_eq(&mgr, 20), vec![key], "new rank indexed");
}

/// The built-in `$expiration` field is range-indexed: entities are findable by
/// a numeric bound on their expiry block, exactly like a user uint attribute.
#[test]
fn expiration_is_range_queryable() {
    let exec = ArkivExecutor::<Mgr>::new();
    let mut db = EmptyDB::default();
    let mut mgr = chain_manager(&mut db, BlockRef::new(9, [0; 32]), Store::default()).unwrap();
    let alice = [0xAA; 20];

    let mut make = |key_byte: u8, expires_at: u64| {
        run(
            &exec,
            &mut mgr,
            alice,
            10,
            &[Op::Create {
                key: [key_byte; 32],
                expires_at,
                creation_flags: CreationFlags::NONE,
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
    assert_eq!(
        mgr.get_within_range(
            EXPIRATION,
            Bound::Included(&uint(100)),
            Bound::Unbounded,
            ReadMode::ViewOnBase,
        )
        .unwrap(),
        vec![[2u8; 32], [3u8; 32]],
    );
}
