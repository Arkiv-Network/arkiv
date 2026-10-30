//! The pruning [`GolemDbStore::open`] takes reaches GolemDB: the space a database takes after
//! commits through [`GolemDbStore`] follows how much history it keeps.

use arkiv_golemdb::{
    CellLimits, Genesis, GolemDbConfig, GolemDbStore, HashAlgorithm, PricingConfig, PruningConfig,
    RecordLimits,
};
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, Entity};
use arkiv_interfaces::entity_records;
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use arkiv_interfaces::store::Store;
use golemdb_storage::Store as _;
use golemdb_storage_mdbx::MdbxStore;
use tempfile::TempDir;

/// The window the middle run keeps.
const WINDOW: u64 = 100;

/// Number of commits in tests.
/// Old data leaves the window and then waits up to [`MAX_WAIT_COMMITS`](golemdb_pruning::MAX_WAIT_COMMITS) to go.
const COMMITS: u16 = (WINDOW + 10 * golemdb_pruning::MAX_WAIT_COMMITS) as u16;

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
        segments: vec![],
    }
}

/// Entity `n`, created in a commit of its own. Its `rank` is its own, and its
/// `team` everyone's, so each commit grows the one `team` bitmap.
fn entity(n: u16) -> Entity {
    let mut key = [0xee; 32];
    key[30..].copy_from_slice(&n.to_be_bytes());
    Entity {
        key,
        owner: [1; 20],
        expires_at: 1_000,
        attributes: vec![
            Attribute::new("rank", AttributeValue::Int(n.into())),
            Attribute::new("team", AttributeValue::Str("red".into())),
        ],
        ..Entity::default()
    }
}

/// The bytes MDBX's pages in use span after [`COMMITS`] commits, each creating
/// one entity. The file itself grows in steps and never shrinks, but pages
/// pruning frees are used again.
fn db_size(pruning: Option<PruningConfig>) -> u64 {
    let dir = TempDir::new().unwrap();
    let store = GolemDbStore::open(dir.path(), &config(), pruning).unwrap();
    for n in 1..=COMMITS {
        let entity = entity(n);
        let branch = store.begin(None).unwrap();
        store
            .create(
                branch,
                entity_records::record_key(entity.key).unwrap(),
                entity_records::to_cells(&entity).unwrap(),
                None,
            )
            .unwrap();
        store.commit(branch).unwrap();
    }
    drop(store);
    let mdbx = MdbxStore::open(dir.path()).unwrap();
    mdbx.stats().unwrap().unwrap().db_size
}

/// Head-only takes less space than a window of [`WINDOW`], which takes less
/// than an archive.
#[test]
fn pruning_frees_space() {
    let archive = db_size(None);
    let head_only = db_size(Some(PruningConfig::new(0)));
    let window = db_size(Some(PruningConfig::new(WINDOW)));
    assert!(
        head_only < window && window < archive,
        "head-only {head_only}, window {window}, archive {archive} bytes"
    );
}
