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
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use arkiv_interfaces::statemanager::{BlockRef, SessionId};
use arkiv_interfaces::store::Store;

/// Arkiv's limits, as the node opens GolemDB with them.
fn config() -> GolemDbConfig {
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
            arkiv_golemdb::PricingConfig::zero(),
        ),
        segments: vec![],
    }
}

/// A view on a fresh in-memory database.
fn view() -> GolemStateView<GolemDbStore> {
    let store = GolemDbStore::open_memory(&config()).expect("open golemdb");
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
