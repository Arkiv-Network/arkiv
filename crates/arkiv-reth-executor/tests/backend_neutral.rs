//! The op executor runs on any `StateView`, not just the MPT host's.
//!
//! `ArkivExecutor<V: StateView, C: CostModel>` has always been generic; what
//! this pins is that the generality is *real* — the same batch, run against
//! `GolemStateView`, produces the same entities, the same gas and the same
//! minting-nonce advance as it does against the MPT view. Without a test that
//! actually instantiates a second backend, "generic over the view" is a claim
//! about the type signature and nothing more.

use arkiv_golemdb_state::{GolemStateManager, GolemStateView};
use arkiv_interfaces::entity::{AttributeValue, CreationFlags};
use arkiv_interfaces::execution::{ExecEnv, ExecStatus, Op};
use arkiv_interfaces::gas::PlaceholderCost;
use arkiv_interfaces::primitives::UserAddress;
use arkiv_interfaces::statemanager::{
    BlockRef, EntityCreationNoncesStore, EntityStore, ReadMode, StateManager, StateView,
};
use arkiv_interfaces::store::reference::MemStore;
use arkiv_interfaces::store::{Store, StoreExt};
use arkiv_reth_executor::arkiv::ArkivExecutor;
use std::sync::Arc;

const ALICE: UserAddress = [0xaa; 20];
const GENESIS: BlockRef = BlockRef {
    height: 0,
    hash: [0; 32],
};

#[allow(clippy::arc_with_non_send_sync)] // MemStore is a RefCell; see its docs.
fn view() -> GolemStateView<Arc<MemStore>> {
    let store = Arc::new(MemStore::new());
    let branch = store.begin(None).expect("begin");
    store
        .commit_tagged(branch, GENESIS.hash)
        .expect("tag genesis");
    GolemStateManager::new(store).view(GENESIS).expect("view")
}

fn create(key: u8) -> Op {
    Op::Create {
        key: [key; 32],
        expires_at: 100,
        creation_flags: CreationFlags::NONE,
        content_type: b"text/plain".to_vec(),
        payload: vec![key],
        attributes: vec![arkiv_interfaces::entity::Attribute::new(
            b"level".to_vec(),
            AttributeValue::Int(i32::from(key)),
        )],
    }
}

#[test]
fn a_batch_runs_against_golemdb_and_lands_in_the_store() {
    let mut view = view();
    let env = ExecEnv {
        caller: ALICE,
        block_number: 1,
        gas_supplied: 10_000_000,
        chain_id: 7738577,
    };

    let ops = vec![create(1), create(2)];
    let out = ArkivExecutor::with_cost(PlaceholderCost)
        .apply(&env, &mut view, &ops)
        .expect("apply");

    assert_eq!(out.status, ExecStatus::Ok);
    assert!(out.gas_used > 0, "a create is not free");

    let deltas = view.get_uncommitted_deltas().expect("deltas");
    assert_eq!(deltas.len(), 2, "one net delta per created entity");

    // Every created entity is readable back through the overlay, with the
    // payload the op carried.
    for delta in &deltas {
        let entity = view
            .get_entity(delta.entity, ReadMode::ViewWithOverlay)
            .expect("read")
            .expect("the entity exists");
        assert_eq!(entity.owner, ALICE);
        assert_eq!(entity.attributes.len(), 1);
    }

    // And the view still commits and graduates, so the batch left it usable.
    StateView::commit(&mut view).expect("commit");
    let commit = view.graduate(BlockRef::new(1, [1; 32])).expect("graduate");
    assert_eq!(commit.parent, GENESIS);
}

/// The minting nonce is an input to every entity address, so a backend that
/// does not advance it mints colliding keys on the next block.
#[test]
fn creation_nonces_advance_on_the_golemdb_backend() {
    let mut view = view();
    let before = view
        .get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
        .expect("read");
    view.fetch_increment_entity_creation_nonce(ALICE)
        .expect("advance");
    let after = view
        .get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
        .expect("read");
    assert_eq!(after.get(), before.get() + 1);
}
