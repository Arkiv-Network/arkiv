//! End-to-end write path with real reth types: ops → view → commit → the
//! `EvmState` commits the authenticated root, with entity bytes outside accounts.

use alloy_primitives::B256;
use arkiv_authenticated_store::{State, Store as Records};
use arkiv_interfaces::entity::{AttributeValue, CreationFlags, annotations};
use arkiv_interfaces::execution::{AttributeMutation, ExecEnv, ExecStatus, Op};
use arkiv_interfaces::statemanager::{BlockRef, EntityStore, ReadMode, StateView};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_reth_statemanager::{
    authenticated::{ROOT_ACCOUNT, root_slot},
    chain::{ChainView, chain_manager},
};
use reth_ethereum::evm::revm::{database_interface::EmptyDB, state::EvmState};

type Store<'a> = ChainView<'a, EmptyDB>;
fn records() -> Records {
    static RECORDS: std::sync::OnceLock<Records> = std::sync::OnceLock::new();
    RECORDS.get_or_init(Records::default).clone()
}
fn store(db: &mut EmptyDB) -> Store<'_> {
    chain_manager(db, BlockRef::new(9, [0; 32]), records()).unwrap()
}
fn committed(diff: &EvmState) -> State<Records> {
    assert_eq!(
        diff.len(),
        1,
        "entity operations only write the root account"
    );
    let account = &diff[&ROOT_ACCOUNT];
    assert_eq!(account.info.nonce, 1);
    assert!(account.info.code.as_ref().is_none_or(|c| c.is_empty()));
    State::open(
        records(),
        B256::from(account.storage[&root_slot()].present_value),
    )
    .unwrap()
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
fn create_commits_the_authenticated_entity() {
    let alice = [0xAA; 20];
    let key = [1u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
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

    // 2) Persist records and commit the root through WriteOverlay.
    StateView::commit(&mut store).unwrap();

    // 3) Read-your-own-writes: the committed entity reads back from the base.
    assert_eq!(
        store
            .get_entity(key, ReadMode::ViewOnBase)
            .unwrap()
            .as_ref(),
        Some(&staged)
    );

    // 4) The native diff contains only the authenticated root account.
    let diff = store.into_base().into_state();
    assert_eq!(committed(&diff).entity(key).unwrap(), Some(staged));
}

#[test]
fn update_recommits_the_authenticated_entity() {
    let alice = [0xAA; 20];
    let key = [5u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
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
    let diff = store.into_base().into_state();
    assert_eq!(committed(&diff).entity(key).unwrap(), Some(got));
}

#[test]
fn transfer_recommits_with_the_new_owner() {
    let alice = [0xAA; 20];
    let bob = [0xBB; 20];
    let key = [6u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
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
    let mut store = store(&mut db);
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
fn delete_removes_the_authenticated_entity() {
    let alice = [0xAA; 20];
    let key = [3u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
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
    let diff = store.into_base().into_state();
    assert!(committed(&diff).entity(key).unwrap().is_none());
}
