//! A query in Arkiv's query language, run against GolemDB on disk the way
//! `arkiv_query` runs it: parsed, lowered to a store filter, answered by
//! `Store::query` or `Store::count`, and read back as entities.
//!
//! The RPC also bounds every query to live entities, `AND $expiresAt >
//! block`: `a_count_combines_predicates` counts in that shape.

use arkiv_golemdb::{CellLimits, GolemDbConfig, GolemDbStore, HashAlgorithm};
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, Entity};
use arkiv_interfaces::entity_records;
use arkiv_interfaces::store::{Filter, Page, Query, Store, StoreError};
use arkiv_query::lower::{Lowered, lower};
use tempfile::TempDir;

/// Arkiv's limits, as the node opens GolemDB with them.
fn config() -> GolemDbConfig {
    let limit = |bytes: usize| u32::try_from(bytes).unwrap();
    GolemDbConfig {
        hash_function: HashAlgorithm::Keccak256,
        cell_limits: CellLimits {
            max_cell_name_len: limit(MAX_ATTRIBUTE_NAME_BYTES),
            max_str_len: limit(MAX_STR_BYTES),
            max_bytes_len: limit(MAX_BYTES_VALUE_LEN),
        },
        segments: vec![],
    }
}

/// Entity `n`, on `team`.
fn entity(n: u8, team: &str) -> Entity {
    Entity {
        key: [n; 32],
        owner: [n; 20],
        expires_at: 1_000,
        attributes: vec![Attribute::new("team", AttributeValue::Str(team.into()))],
        ..Entity::default()
    }
}

/// A store holding `entities`, committed.
fn store_with(dir: &TempDir, entities: &[Entity]) -> GolemDbStore {
    let store = GolemDbStore::open(dir.path(), &config()).unwrap();
    let branch = store.begin(None).unwrap();
    for entity in entities {
        let cells = entity_records::to_cells(entity).unwrap();
        store
            .create(branch, entity_records::record_key(entity.key), cells, None)
            .unwrap();
    }
    store.commit(branch).unwrap();
    store
}

/// `text` as the store filter `arkiv_query` would lower it to.
fn filter(text: &str) -> Filter {
    match lower(&arkiv_query::parse(text).unwrap()).unwrap() {
        Lowered::Filter(filter) => filter,
        Lowered::Nothing => panic!("`{text}` matches nothing"),
    }
}

#[test]
fn a_simple_query_returns_the_entities_it_matches() {
    let dir = TempDir::new().unwrap();
    let (red, blue, also_red) = (entity(1, "red"), entity(2, "blue"), entity(3, "red"));
    let store = store_with(&dir, &[red.clone(), blue, also_red.clone()]);

    let query = Query {
        filter: filter("team = str('red')"),
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };
    let result = store
        .query(None, &query, None)
        .expect("GolemDB runs a one-predicate query")
        .into_value();
    let found: Vec<Entity> = result
        .records
        .iter()
        .map(|record| entity_records::from_record(record).unwrap())
        .collect();
    assert_eq!(found, [red, also_red]);
}

#[test]
fn a_simple_count_counts_the_entities_it_matches() {
    let dir = TempDir::new().unwrap();
    let store = store_with(
        &dir,
        &[entity(1, "red"), entity(2, "blue"), entity(3, "red")],
    );

    let count = |text| {
        store
            .count(None, &filter(text), None)
            .expect("GolemDB runs a one-predicate count")
            .into_value()
    };
    assert_eq!(count("team = str('red')"), 2);
    assert_eq!(count("team = str('blue')"), 1);
    assert_eq!(count("team = str('green')"), 0);
}

/// The shapes `arkiv_query` sends: bounded to live entities by an AND, and
/// ORs. A NOT has no GolemDB counterpart.
#[test]
fn a_count_combines_predicates() {
    let dir = TempDir::new().unwrap();
    let store = store_with(
        &dir,
        &[entity(1, "red"), entity(2, "blue"), entity(3, "red")],
    );

    let count = |text| {
        store
            .count(None, &filter(text), None)
            .map(|count| count.into_value())
    };
    assert_eq!(count("team = str('red') AND $expiresAt > u64(500)"), Ok(2));
    assert_eq!(count("team = str('red') AND $expiresAt > u64(1000)"), Ok(0));
    assert_eq!(count("team = str('red') OR team = str('blue')"), Ok(3));
    assert_eq!(count("team = str('blue') OR $expiresAt > u64(1000)"), Ok(1));
    assert_eq!(
        count("NOT (team = str('red'))"),
        Err(StoreError::NotImplemented)
    );
}
