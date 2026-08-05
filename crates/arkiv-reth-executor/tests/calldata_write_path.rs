//! Client calldata → committed reth state, end to end.
//!
//! Extends the entity write-path test one seam further *upstream*: it starts from
//! real `execute(Operation[])` ABI calldata (built with the SDK bindings, exactly
//! what `arkiv-cli` would send), decodes it, runs it, and commits it — proving the
//! whole path a client sees:
//!
//! ```text
//!   Operation[] calldata  →  decode_ops  →  Op[]
//!     → ArkivExecutor::apply over RethEntityStore<CodeBackend<ExecutorState>>
//!     → apply_delta → EvmState → the entity is committed at its minted key
//! ```

use alloy_sol_types::SolCall;
use arkiv_bindings::{Attribute as AbiAttribute, IEntityRegistry, Ident32, Operation};
use arkiv_interfaces::execution::{BlockDraft, ExecEnv, ExecStatus};
use arkiv_interfaces::primitives::EntityNonce;
use arkiv_interfaces::state::EntityStore;
use arkiv_reth_entitystore::layout::entity_address;
use arkiv_reth_entitystore::{CodeBackend, RethEntityStore, decode};
use arkiv_reth_executor::{ArkivExecutor, ExecutorState, decode_ops, derive_entity_key};
use reth_ethereum::evm::revm::database_interface::EmptyDB;

const CHAIN_ID: u64 = 1;

/// The `$payload` triple — payload rides in the attribute list now.
fn payload_attr(bytes: &[u8]) -> AbiAttribute {
    AbiAttribute::from_value(
        Ident32::system("$payload").unwrap(),
        &arkiv_interfaces::entity::AttributeValue::Bytes(bytes.to_vec()),
    )
    .unwrap()
}

/// A create with a purely relative lifetime.
fn create(min_lifetime: u64, payload: &[u8]) -> Operation {
    Operation::create(0, 0, min_lifetime, 0, vec![payload_attr(payload)])
}

fn calldata(ops: Vec<Operation>) -> Vec<u8> {
    IEntityRegistry::executeCall { ops }.abi_encode()
}

fn env(caller: [u8; 20], block: u64) -> ExecEnv {
    ExecEnv {
        caller,
        block_number: block,
        gas_supplied: 100_000_000,
        chain_id: CHAIN_ID,
    }
}

#[test]
fn create_from_calldata_lands_in_reth_state() {
    let alice = [0xAA; 20];
    // What arkiv-cli would send: a create with a 50-block TTL. entityKey is 0 in the
    // calldata — the node mints it.
    let cd = calldata(vec![create(50, b"hello")]);

    let env = env(alice, 10);
    let ops = decode_ops(&env, &cd, EntityNonce::new(0)).unwrap(); // start_nonce 0

    let mut db = EmptyDB::default();
    let mut store = RethEntityStore::new(CodeBackend::new(ExecutorState::new(&mut db)));
    let mut draft = BlockDraft::default();
    let exec = ArkivExecutor::new();

    let out = exec.apply(&env, &mut store, &mut draft, &ops).unwrap();
    assert_eq!(out.status, ExecStatus::Ok);

    // The entity was minted at the derived key, with env-resolved lifecycle fields.
    let expected_key = derive_entity_key(CHAIN_ID, &alice, EntityNonce::new(0), 0);
    let staged = draft.entities.puts[0].clone();
    assert_eq!(staged.key, expected_key);
    assert_eq!(staged.owner, alice);
    assert_eq!(staged.expires_at, 60); // block 10 + btl 50
    assert_eq!(staged.payload, b"hello");

    // Commit, and confirm it reads back and is the committed account's code.
    store.apply_delta(&draft.entities).unwrap();
    assert_eq!(store.get(expected_key).unwrap().as_ref(), Some(&staged));

    let diff = store.into_backend().into_inner().into_state();
    let acc = diff
        .get(&entity_address(expected_key))
        .expect("entity account in the diff");
    let code = acc.info.code.as_ref().expect("has code").original_bytes();
    assert_eq!(decode(&code).unwrap(), staged);
}

#[test]
fn a_batch_of_creates_lands_each_at_its_minted_key() {
    let alice = [0xAA; 20];
    let cd = calldata(vec![create(10, b"a"), create(10, b"b")]);

    let env = env(alice, 5);
    let ops = decode_ops(&env, &cd, EntityNonce::new(0)).unwrap();

    let mut db = EmptyDB::default();
    let mut store = RethEntityStore::new(CodeBackend::new(ExecutorState::new(&mut db)));
    let mut draft = BlockDraft::default();
    let exec = ArkivExecutor::new();
    exec.apply(&env, &mut store, &mut draft, &ops).unwrap();
    store.apply_delta(&draft.entities).unwrap();

    // Two distinct keys from consecutive nonces, each holding its own entity.
    let k0 = derive_entity_key(CHAIN_ID, &alice, EntityNonce::new(0), 0);
    let k1 = derive_entity_key(CHAIN_ID, &alice, EntityNonce::new(1), 0);
    assert_ne!(k0, k1);
    assert_eq!(store.get(k0).unwrap().unwrap().payload, b"a");
    assert_eq!(store.get(k1).unwrap().unwrap().payload, b"b");
}
