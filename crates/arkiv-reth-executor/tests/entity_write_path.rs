//! End-to-end write path with real reth types: ops → view → commit → the
//! `EvmState` diff holds the entity as account code, byte-for-byte.

use alloy_primitives::B256;
use arkiv_interfaces::entity::{AttributeValue, CreationFlags, annotations};
use arkiv_interfaces::execution::{AttributeMutation, ExecEnv, ExecStatus, Op};
use arkiv_interfaces::statemanager::{BlockRef, EntityStore, ReadMode, StateView};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_reth_statemanager::{WriteManager, write_manager};
use arkiv_store::{ARKIV_ROOT_ACCOUNT, ARKIV_ROOT_SLOT, DbView, NodeStore, SharedMemNodeStore};
use reth_ethereum::evm::revm::database_interface::EmptyDB;
use reth_ethereum::evm::revm::state::EvmState;

type Store<'a> = WriteManager<'a, EmptyDB, SharedMemNodeStore>;

fn store<'a>(db: &'a mut EmptyDB, nodes: &'a SharedMemNodeStore) -> Store<'a> {
    write_manager(db, nodes, BlockRef::new(9, [0; 32])).unwrap()
}

/// Close the view as the executor does: flush its nodes, take the diff.
fn finish(store: Store<'_>, nodes: &SharedMemNodeStore) -> EvmState {
    let (overlay, staging) = store.into_parts();
    nodes.flush(staging.into_staged()).unwrap();
    overlay.into_state()
}

/// The database root the diff's anchor slot names, and its view.
fn db_of<'a>(diff: &EvmState, nodes: &'a SharedMemNodeStore) -> DbView<'a, SharedMemNodeStore> {
    let acc = diff
        .get(&ARKIV_ROOT_ACCOUNT)
        .expect("anchor account staged in the diff");
    assert!(acc.is_touched());
    assert_eq!(acc.info.nonce, 1, "the anchor is kept alive");
    let root = B256::from(
        acc.storage
            .get(&ARKIV_ROOT_SLOT)
            .unwrap()
            .present_value
            .to_be_bytes::<32>(),
    );
    DbView::open(nodes, root).unwrap()
}

fn env(caller: [u8; 20], block: u64) -> ExecEnv {
    ExecEnv {
        caller,
        block_number: block,
        gas_supplied: 100_000_000,
        chain_id: 1,
    }
}

/// Run `ops` and commit the view — one transaction's worth of writes.
fn run<'a>(
    exec: &ArkivExecutor<Store<'a>>,
    store: &mut Store<'a>,
    caller: [u8; 20],
    block: u64,
    ops: &[Op],
) {
    let out = exec.apply(&env(caller, block), store, ops).unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    StateView::commit(store).unwrap();
}

#[test]
fn create_commits_the_entity_to_the_database() {
    let alice = [0xAA; 20];
    let key = [1u8; 32];

    let mut db = EmptyDB::default();
    let nodes = SharedMemNodeStore::new();
    let mut store = store(&mut db, &nodes);
    let exec = ArkivExecutor::new();

    // 1) Run the STF — the entity stages into the view's overlay.
    let out = exec
        .apply(
            &env(alice, 10),
            &mut store,
            &[Op::Create {
                key,
                expires_at: 50,
                creation_flags: CreationFlags::NONE,
                content_type: b"text/plain".to_vec(),
                payload: b"hello".to_vec(),
                attributes: Vec::new(),
            }],
        )
        .unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    let staged = store
        .get_entity(key, ReadMode::ViewWithOverlay)
        .unwrap()
        .expect("staged in the view");
    assert_eq!(staged.owner, alice); // lifecycle fields came from the env
    assert_eq!(staged.created_at_block, 10);
    assert!(
        store
            .get_entity(key, ReadMode::ViewOnBase)
            .unwrap()
            .is_none(),
        "nothing reaches the base before commit"
    );

    // 2) Commit — flows through CodeBackend → WriteOverlay → EvmState.
    StateView::commit(&mut store).unwrap();

    // 3) Read-your-own-writes: the committed entity reads back from the base.
    assert_eq!(
        store
            .get_entity(key, ReadMode::ViewOnBase)
            .unwrap()
            .as_ref(),
        Some(&staged)
    );

    // 4) The recovered EvmState diff is what reth would commit: the entity
    //    account exists, is touched, survives EIP-161, and holds the record.
    let diff = finish(store, &nodes);
    let view = db_of(&diff, &nodes);
    assert_eq!(
        view.entity(&key).unwrap().as_ref(),
        Some(&staged),
        "the database at the anchored root holds the record"
    );
}

#[test]
fn update_recommits_the_entity_as_new_code() {
    let alice = [0xAA; 20];
    let key = [5u8; 32];

    let mut db = EmptyDB::default();
    let nodes = SharedMemNodeStore::new();
    let mut store = store(&mut db, &nodes);
    let exec = ArkivExecutor::new();

    // Create at block 10, then update the payload at block 11 — two commit
    // cycles on the one view, the second reading the first's committed state.
    run(
        &exec,
        &mut store,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"v1".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(
        &exec,
        &mut store,
        alice,
        11,
        &[Op::Patch {
            key,
            mutations: vec![AttributeMutation::set(
                annotations::PAYLOAD,
                AttributeValue::Bytes(b"v2".to_vec()),
            )],
        }],
    );

    // The committed record is the updated one, with lifecycle fields preserved.
    let got = store
        .get_entity(key, ReadMode::ViewOnBase)
        .unwrap()
        .expect("entity present");
    assert_eq!(got.payload, b"v2");
    assert_eq!(got.created_at_block, 10, "create block preserved");
    assert_eq!(
        got.last_modified_at_block, 11,
        "patch advances lastModified"
    );
    let diff = finish(store, &nodes);
    let view = db_of(&diff, &nodes);
    assert_eq!(
        view.entity(&key).unwrap(),
        Some(got),
        "the anchored database holds the update"
    );
}

#[test]
fn transfer_recommits_with_the_new_owner() {
    let alice = [0xAA; 20];
    let bob = [0xBB; 20];
    let key = [6u8; 32];

    let mut db = EmptyDB::default();
    let nodes = SharedMemNodeStore::new();
    let mut store = store(&mut db, &nodes);
    let exec = ArkivExecutor::new();

    run(
        &exec,
        &mut store,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(
        &exec,
        &mut store,
        alice,
        11,
        &[Op::Transfer {
            key,
            new_owner: bob,
        }],
    );

    let got = store
        .get_entity(key, ReadMode::ViewOnBase)
        .unwrap()
        .expect("entity present");
    assert_eq!(got.owner, bob, "owner changed");
    assert_eq!(got.creator, alice, "creator is immutable");
}

#[test]
fn extend_recommits_with_a_higher_expiry() {
    let alice = [0xAA; 20];
    let key = [7u8; 32];

    let mut db = EmptyDB::default();
    let nodes = SharedMemNodeStore::new();
    let mut store = store(&mut db, &nodes);
    let exec = ArkivExecutor::new();

    run(
        &exec,
        &mut store,
        alice,
        10,
        &[Op::Create {
            key,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    let before = store
        .get_entity(key, ReadMode::ViewOnBase)
        .unwrap()
        .unwrap()
        .expires_at;

    run(
        &exec,
        &mut store,
        alice,
        11,
        &[Op::ExtendExpiry {
            key,
            new_expires_at: 500,
        }],
    );

    let after = store
        .get_entity(key, ReadMode::ViewOnBase)
        .unwrap()
        .unwrap()
        .expires_at;
    assert!(after > before, "extend raises expiry: {before} -> {after}");
    assert_eq!(after, 500);
}

#[test]
fn delete_commits_a_tombstone() {
    let alice = [0xAA; 20];
    let key = [3u8; 32];

    let mut db = EmptyDB::default();
    let nodes = SharedMemNodeStore::new();
    let mut store = store(&mut db, &nodes);
    let exec = ArkivExecutor::new();

    // Create-then-delete in one batch nets to nothing at all.
    let out = exec
        .apply(
            &env(alice, 7),
            &mut store,
            &[
                Op::Create {
                    key,
                    expires_at: 50,
                    creation_flags: CreationFlags::NONE,
                    content_type: b"x".to_vec(),
                    payload: b"y".to_vec(),
                    attributes: Vec::new(),
                },
                Op::Delete { key },
            ],
        )
        .unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    assert!(store.get_uncommitted_deltas().unwrap().is_empty());

    // A committed entity deleted in a later transaction tombstones its account:
    // no entity reads back, and the account holds no code (kept alive against
    // pruning).
    run(
        &exec,
        &mut store,
        alice,
        8,
        &[Op::Create {
            key,
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    );
    run(&exec, &mut store, alice, 9, &[Op::Delete { key }]);

    assert!(
        store
            .get_entity(key, ReadMode::ViewOnBase)
            .unwrap()
            .is_none()
    );
    let diff = finish(store, &nodes);
    let view = db_of(&diff, &nodes);
    assert_eq!(
        view.entity(&key).unwrap(),
        None,
        "deleted from the anchored database"
    );
}
