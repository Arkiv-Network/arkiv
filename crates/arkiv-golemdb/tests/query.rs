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

/// Entity `n`, carrying exactly `attributes`.
fn entity_with(n: u8, attributes: Vec<Attribute>) -> Entity {
    Entity {
        key: [n; 32],
        owner: [n; 20],
        expires_at: 1_000,
        attributes,
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

/// The entities that tell the reserved operators apart: two carrying `team` as
/// a `str`, one not carrying it at all, and one carrying it as an `i32`.
fn mixed_team_attribute() -> Vec<Entity> {
    vec![
        entity(1, "red"),
        entity(2, "blue"),
        entity_with(3, vec![Attribute::new("level", AttributeValue::Int(7))]),
        entity_with(4, vec![Attribute::new("team", AttributeValue::Int(7))]),
    ]
}

/// The first byte of each matched record's key — the entity numbers, in page
/// order.
fn matched(store: &GolemDbStore, filter: Filter) -> Vec<u8> {
    let query = Query {
        filter,
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };
    store
        .query(None, &query, None)
        .expect("GolemDB answers the query")
        .into_value()
        .records
        .iter()
        .map(|record| record.key.0[0])
        .collect()
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

/// A page is `offset` matches in, `limit` long. The matches are in key order,
/// so the pages partition them: walking a limit at a time yields every match
/// once, in order, and an offset past the last one is an empty page, not an
/// error. `total_matched` is the whole match set, independent of the page.
///
/// GolemDB caps a page at its own query limit; over that, a page is
/// `LimitExceeded` rather than a silent trim.
#[test]
fn a_query_pages_by_limit_and_offset() {
    let dir = TempDir::new().unwrap();
    let all = [
        entity(1, "red"),
        entity(2, "red"),
        entity(3, "red"),
        entity(4, "red"),
        entity(5, "red"),
    ];
    let store = store_with(&dir, &all);

    let page = |offset, limit, total_matched| {
        let query = Query {
            filter: filter("team = str('red')"),
            page: Page { offset, limit },
            total_matched,
            ..Query::default()
        };
        store
            .query(None, &query, None)
            .expect("GolemDB pages a one-predicate query")
            .into_value()
    };
    let keys = |offset, limit| {
        page(offset, limit, false)
            .records
            .iter()
            .map(|record| record.key.0[0])
            .collect::<Vec<u8>>()
    };

    // A limit that holds everything, and one that holds nothing left to hold.
    assert_eq!(keys(0, 10), [1, 2, 3, 4, 5]);
    assert!(keys(5, 10).is_empty());
    assert!(keys(9, 10).is_empty());

    // Successive pages partition the matches, in key order.
    assert_eq!(keys(0, 2), [1, 2]);
    assert_eq!(keys(2, 2), [3, 4]);
    assert_eq!(keys(4, 2), [5]);
    assert!(keys(6, 2).is_empty());

    // An offset with no limit left over, and a limit of one.
    assert_eq!(keys(1, 3), [2, 3, 4]);
    assert_eq!(keys(0, 1), [1]);
    assert_eq!(keys(4, 1), [5]);

    // The count is the whole match set, not the page.
    assert_eq!(page(0, 2, true).total_matched, Some(5));
    assert_eq!(page(4, 2, true).total_matched, Some(5));
    // And it is absent unless asked for.
    assert_eq!(page(0, 2, false).total_matched, None);

    // A page over GolemDB's own query limit is a structural limit.
    let query = Query {
        filter: filter("team = str('red')"),
        page: Page {
            offset: 0,
            limit: 1_000,
        },
        ..Query::default()
    };
    assert_eq!(
        store
            .query(None, &query, None)
            .map(|page| page.into_value()),
        Err(StoreError::LimitExceeded)
    );
}

/// The fixture the two red tests below rest on, checked with operators that
/// work today: one attribute name carries two types at once, and a typed value
/// predicate sees only its own type. Without this, a failure there could be the
/// fixture rather than the missing operator.
#[test]
fn one_attribute_name_carries_two_types_at_once() {
    let dir = TempDir::new().unwrap();
    let store = store_with(&dir, &mixed_team_attribute());

    assert_eq!(matched(&store, filter("team = str('red')")), [1]);
    assert_eq!(matched(&store, filter("team = i32(7)")), [4]);
    assert_eq!(matched(&store, filter("level = i32(7)")), [3]);
    // The `i32` holder is invisible to a `str` predicate, and vice versa.
    assert!(matched(&store, filter("team = str('7')")).is_empty());
}

/// `exists(attr)` matches every record carrying `attr`, whatever its type,
/// and only those: a record without the attribute does not match, and one
/// carrying it under another type does.
///
/// Lowers to a store presence test, which GolemDB answers with its own
/// `Exists` leaf.
#[test]
fn a_query_with_the_exists_operator_matches_the_records_that_have_the_attribute() {
    let dir = TempDir::new().unwrap();
    let store = store_with(&dir, &mixed_team_attribute());

    // Type-blind: both `str` holders and the `i32` holder match.
    assert_eq!(matched(&store, filter("exists(team)")), [1, 2, 4]);
    // The entity that carries `level` instead is the only one `exists(level)`
    // matches.
    assert_eq!(matched(&store, filter("exists(level)")), [3]);
    // As one half of an AND whose other half is answerable today.
    assert_eq!(
        matched(&store, filter("exists(team) AND $expiresAt > u64(500)")),
        [1, 2, 4]
    );
    // No record carries both attributes.
    assert!(matched(&store, filter("exists(team) AND exists(level)")).is_empty());
}

/// `typeof(attr) = type` narrows presence to one type: it matches the records
/// carrying `attr` under exactly that type, so it splits the records that
/// `exists(attr)` matches together.
///
/// The same leaf as `exists`, narrowed to one type.
#[test]
fn a_query_with_the_typeof_operator_matches_the_records_whose_attribute_has_that_type() {
    let dir = TempDir::new().unwrap();
    let store = store_with(&dir, &mixed_team_attribute());

    assert_eq!(matched(&store, filter("typeof(team) = str")), [1, 2]);
    assert_eq!(matched(&store, filter("typeof(team) = i32")), [4]);
    assert_eq!(matched(&store, filter("typeof(level) = i32")), [3]);
    // A type no record carries the attribute under matches nothing, rather
    // than failing.
    assert!(matched(&store, filter("typeof(team) = u64")).is_empty());
    // Narrower than a value predicate on the same attribute, and wider than
    // one on a single value.
    assert_eq!(
        matched(&store, filter("typeof(team) = str AND team = str('red')")),
        [1]
    );
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
