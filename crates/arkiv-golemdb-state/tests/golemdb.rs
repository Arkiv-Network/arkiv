//! [`GolemStateView`] over the real GolemDB engine.
//!
//! The unit tests run over `MemStore`, whose write details are computed by the
//! shared size rule rather than measured. GolemDB is the reference that rule is
//! written against, so the shared `StateView` contract runs here too: the entity
//! write details and frame rules it asserts must hold on the engine itself.

use arkiv_golemdb::{
    CellLimits, Genesis, GolemDbConfig, GolemDbStore, HashAlgorithm, RecordLimits,
};
use arkiv_golemdb_state::GolemStateView;
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, Entity};
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use arkiv_interfaces::statemanager::{
    BlockRef, EntityCreationNoncesStore, EntityStore, EntityUpdates, ReadMode, SessionId,
};
use arkiv_interfaces::store::Store;

/// Arkiv's limits, as the node opens GolemDB with them.
fn config() -> GolemDbConfig {
    config_with(arkiv_golemdb::PricingConfig::zero())
}

fn config_with(pricing: arkiv_golemdb::PricingConfig) -> GolemDbConfig {
    let limit = |bytes: usize| u32::try_from(bytes).unwrap();
    let limits = CellLimits {
        max_cell_name_len: limit(MAX_ATTRIBUTE_NAME_BYTES),
        max_str_len: limit(MAX_STR_BYTES),
        max_bytes_len: limit(MAX_BYTES_VALUE_LEN),
    };
    let record_limits = RecordLimits {
        max_cells: MAX_RECORD_CELLS as u64,
        max_indexed_cells: MAX_RECORD_INDEXED_CELLS as u64,
    };
    GolemDbConfig {
        genesis: Genesis::new(
            HashAlgorithm::Keccak256,
            limits,
            record_limits,
            None,
            pricing,
        ),
        segments: vec![],
    }
}

/// A view on a fresh in-memory database.
fn view() -> GolemStateView<GolemDbStore> {
    view_over(config())
}

/// A view where every GolemDB weight is 1, so every call costs something.
fn priced_view() -> GolemStateView<GolemDbStore> {
    let mut pricing = arkiv_golemdb::PricingConfig::zero();
    pricing.weights.values_mut().for_each(|weight| *weight = 1);
    view_over(config_with(pricing))
}

fn view_over(config: GolemDbConfig) -> GolemStateView<GolemDbStore> {
    let store = GolemDbStore::open_memory(&config).expect("open golemdb");
    let origin = store.head();
    let branch = store.begin(Some(origin)).expect("begin");
    GolemStateView::new(
        store,
        branch,
        origin,
        BlockRef::new(10, [0xBB; 32]),
        SessionId([1; 16]),
    )
}

#[test]
fn conformance() {
    arkiv_interfaces::statemanager::conformance::run_all(&view);
}

fn entity(key: u8) -> Entity {
    let mut entity = Entity {
        key: [key; 32],
        owner: [0xaa; 20],
        creator: [0xaa; 20],
        expires_at: 100,
        ..Default::default()
    };
    entity.attributes.push(Attribute::new(
        b"level".to_vec(),
        AttributeValue::Str("seven".into()),
    ));
    entity
}

/// A write reports what it did and what it cost, and a read reports the
/// entity's size.
#[test]
fn an_entity_write_reports_its_effects_and_cost() {
    let mut view = priced_view();
    let created = view
        .update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    assert!(created.receipt.cost.0 > 0);
    assert!(created.receipt.details.cells_created > 0);

    let read = view
        .get_entity_with_meta([1; 32], ReadMode::ViewWithOverlay)
        .unwrap();
    assert!(read.value.expect("the entity").meta.cell_bytes > 0);
}

/// Advancing the minting nonce reads and writes, so it costs more than the read.
#[test]
fn advancing_the_minting_nonce_reports_its_read_and_its_write() {
    let mut view = priced_view();
    let read = view
        .get_entity_creation_nonce([0xaa; 20], ReadMode::ViewWithOverlay)
        .unwrap();
    let advanced = view
        .fetch_increment_entity_creation_nonce([0xaa; 20])
        .unwrap();
    assert!(advanced.receipt.cost.0 > read.receipt.cost.0);
}

/// A discarded frame undoes the writes, not what they cost: the receipt is a
/// return value, so the caller still has it.
#[test]
fn a_discarded_frame_keeps_what_it_spent() {
    let mut view = priced_view();
    view.begin_frame().unwrap();
    let created = view
        .update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    view.discard_frame().unwrap();
    assert!(created.receipt.cost.0 > 0);
    assert!(
        view.get_entity([1; 32], ReadMode::ViewWithOverlay)
            .unwrap()
            .value
            .is_none()
    );
}
