//! `GolemDbStore` against the shared store contract: part one only.
//!
//! Part two of the suite asserts features GolemDB does not implement yet; see
//! [`run_part_two`](arkiv_interfaces::store::conformance::run_part_two). As
//! they land, their assertions move to part one and so start running here.

use arkiv_golemdb::{
    CellLimits, Genesis, GolemDbConfig, GolemDbStore, HashAlgorithm, PricingConfig, RecordLimits,
    SegmentSpec,
};
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use arkiv_interfaces::store::conformance;

/// Arkiv's limits, as the node opens GolemDB with them, and the segments the
/// suite appends to. The suite's rows have one column each.
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
        host_schemas: vec![],
        genesis: Genesis::new(
            HashAlgorithm::Keccak256,
            limits,
            record_limits,
            None,
            PricingConfig::zero(),
        ),
        segments: ["blocks", "receipts", "transactions"]
            .into_iter()
            .map(|name| SegmentSpec {
                name: name.into(),
                columns: 1,
            })
            .collect(),
    }
}

/// A fresh database per assertion, in memory.
fn new_store() -> GolemDbStore {
    GolemDbStore::open_memory(&config()).expect("open GolemDB in memory")
}

#[test]
fn conformance_part_one() {
    conformance::run_part_one(&new_store);
}
