//! End-to-end write-path integration test — the first that crosses every seam
//! with real reth types instead of a mock.
//!
//! ```text
//!   ArkivExecutor::apply(ops)                       → BlockDraft (the entity delta)
//!     reading RethEntityStore<CodeBackend<ExecutorState<EmptyDB>>>
//!   store.apply_delta(&draft.entities)              → CodeBackend → ExecutorState
//!                                                     → EvmState overlay
//!   store.get(key)                                  → reads its own writes back
//!   ...into_state()                                 → the EvmState reth would commit
//! ```
//!
//! It proves the business-logic delta actually becomes correct reth account state:
//! the entity lands as `code` at its address, byte-for-byte and still decodable.

use arkiv_interfaces::execution::{BlockDraft, ExecEnv, ExecStatus, Op};
use arkiv_interfaces::state::EntityStore;
use arkiv_reth_entitystore::layout::entity_address;
use arkiv_reth_entitystore::{CodeBackend, RethEntityStore, decode, encode};
use arkiv_reth_executor::{ArkivExecutor, ExecutorState};
use reth_ethereum::evm::revm::database_interface::EmptyDB;

type Store<'a> = RethEntityStore<CodeBackend<ExecutorState<'a, EmptyDB>>>;

fn store(db: &mut EmptyDB) -> Store<'_> {
    RethEntityStore::new(CodeBackend::new(ExecutorState::new(db)))
}

fn env(caller: [u8; 20], block: u64) -> ExecEnv {
    ExecEnv {
        caller,
        block_number: block,
        gas_supplied: 100_000_000,
        chain_id: 1,
    }
}

#[test]
fn create_commits_the_entity_as_account_code() {
    let alice = [0xAA; 20];
    let key = [1u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
    let mut draft = BlockDraft::default();
    let exec = ArkivExecutor::new();

    // 1) Run the STF over the reth-backed store — stages the entity into the draft.
    let out = exec
        .apply(
            &env(alice, 10),
            &mut store,
            &mut draft,
            &[Op::Create {
                key,
                expires_at: 50,
                content_type: b"text/plain".to_vec(),
                payload: b"hello".to_vec(),
                attributes: Vec::new(),
            }],
        )
        .unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    let staged = draft.entities.puts[0].clone();
    assert_eq!(staged.owner, alice); // lifecycle fields came from the env
    assert_eq!(staged.created_at_block, 10);

    // 2) Commit the delta — flows through CodeBackend → ExecutorState → EvmState.
    store.apply_delta(&draft.entities).unwrap();

    // 3) Full-stack read-your-own-writes: the committed entity reads back by key.
    assert_eq!(store.get(key).unwrap().as_ref(), Some(&staged));

    // 4) The recovered EvmState diff is what reth would commit: the entity account
    //    exists, is touched, survives EIP-161, and holds the record as code —
    //    byte-for-byte and still decodable.
    let diff = store.into_backend().into_inner().into_state();
    let acc = diff
        .get(&entity_address(key))
        .expect("entity account staged in the diff");
    assert!(acc.is_touched());
    assert_eq!(acc.info.nonce, 1);

    let code = acc
        .info
        .code
        .as_ref()
        .expect("account has code")
        .original_bytes();
    assert_eq!(
        code.as_ref(),
        encode(&staged),
        "code is the record, verbatim"
    );
    assert_eq!(decode(&code).unwrap(), staged, "and still decodes");
}

#[test]
fn update_recommits_the_entity_as_new_code() {
    let alice = [0xAA; 20];
    let key = [5u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
    let exec = ArkivExecutor::new();

    // Create at block 10, then update the payload at block 11 — two commit cycles on
    // the one store, the second reading the first's committed state.
    let mut draft = BlockDraft::default();
    exec.apply(
        &env(alice, 10),
        &mut store,
        &mut draft,
        &[Op::Create {
            key,
            expires_at: 100,
            content_type: b"text/plain".to_vec(),
            payload: b"v1".to_vec(),
            attributes: Vec::new(),
        }],
    )
    .unwrap();
    store.apply_delta(&draft.entities).unwrap();

    let mut draft = BlockDraft::default();
    let out = exec
        .apply(
            &env(alice, 11),
            &mut store,
            &mut draft,
            &[Op::Update {
                key,
                content_type: b"text/plain".to_vec(),
                payload: b"v2".to_vec(),
                attributes: Vec::new(),
            }],
        )
        .unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    store.apply_delta(&draft.entities).unwrap();

    // The committed record is the updated one, with lifecycle fields preserved.
    let got = store.get(key).unwrap().expect("entity present");
    assert_eq!(got.payload, b"v2");
    assert_eq!(got.created_at_block, 10, "create block preserved");
    assert_eq!(
        got.last_modified_at_block, 11,
        "update advances lastModified"
    );
    let diff = store.into_backend().into_inner().into_state();
    let code = diff
        .get(&entity_address(key))
        .and_then(|a| a.info.code.as_ref())
        .expect("entity code")
        .original_bytes();
    assert_eq!(decode(&code).unwrap(), got, "code is the updated record");
}

#[test]
fn transfer_recommits_with_the_new_owner() {
    let alice = [0xAA; 20];
    let bob = [0xBB; 20];
    let key = [6u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
    let exec = ArkivExecutor::new();

    let mut draft = BlockDraft::default();
    exec.apply(
        &env(alice, 10),
        &mut store,
        &mut draft,
        &[Op::Create {
            key,
            expires_at: 100,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    )
    .unwrap();
    store.apply_delta(&draft.entities).unwrap();

    let mut draft = BlockDraft::default();
    exec.apply(
        &env(alice, 11),
        &mut store,
        &mut draft,
        &[Op::Transfer {
            key,
            new_owner: bob,
        }],
    )
    .unwrap();
    store.apply_delta(&draft.entities).unwrap();

    let got = store.get(key).unwrap().expect("entity present");
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

    let mut draft = BlockDraft::default();
    exec.apply(
        &env(alice, 10),
        &mut store,
        &mut draft,
        &[Op::Create {
            key,
            expires_at: 100,
            content_type: b"x".to_vec(),
            payload: b"y".to_vec(),
            attributes: Vec::new(),
        }],
    )
    .unwrap();
    store.apply_delta(&draft.entities).unwrap();
    let before = store.get(key).unwrap().unwrap().expires_at;

    let mut draft = BlockDraft::default();
    exec.apply(
        &env(alice, 11),
        &mut store,
        &mut draft,
        &[Op::ExtendExpiry {
            key,
            new_expires_at: 500,
        }],
    )
    .unwrap();
    store.apply_delta(&draft.entities).unwrap();

    let after = store.get(key).unwrap().unwrap().expires_at;
    assert!(after > before, "extend raises expiry: {before} -> {after}");
    assert_eq!(after, 500);
}

#[test]
fn create_then_delete_commits_a_tombstone() {
    let alice = [0xAA; 20];
    let key = [3u8; 32];

    let mut db = EmptyDB::default();
    let mut store = store(&mut db);
    let mut draft = BlockDraft::default();
    let exec = ArkivExecutor::new();

    // Create then delete the same entity in one batch — nets to a delete.
    let out = exec
        .apply(
            &env(alice, 7),
            &mut store,
            &mut draft,
            &[
                Op::Create {
                    key,
                    expires_at: 50,
                    content_type: b"x".to_vec(),
                    payload: b"y".to_vec(),
                    attributes: Vec::new(),
                },
                Op::Delete { key },
            ],
        )
        .unwrap();
    assert_eq!(out.status, ExecStatus::Ok);
    assert!(draft.entities.deletes.contains(&key));

    store.apply_delta(&draft.entities).unwrap();

    // The account is tombstoned: no entity reads back, and the committed account
    // holds no code (kept alive against pruning).
    assert!(store.get(key).unwrap().is_none());
    let diff = store.into_backend().into_inner().into_state();
    let acc = diff
        .get(&entity_address(key))
        .expect("tombstoned account staged in the diff");
    assert!(acc.info.code.as_ref().is_none_or(|c| c.is_empty()));
    assert_eq!(acc.info.nonce, 1);
}
