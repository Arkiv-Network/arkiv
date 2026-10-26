//! A query in Arkiv's query language, run against GolemDB on disk the way
//! `arkiv_query` runs it: parsed, taken node for node as a store filter,
//! answered by `Store::query` or `Store::count`, and read back as entities.
//!
//! The RPC also bounds every query to live entities, `AND $expiresAt >
//! block`: `a_count_combines_predicates` counts in that shape.

use arkiv_golemdb::{
    CellLimits, Genesis, GolemDbConfig, GolemDbStore, HashAlgorithm, RecordLimits,
};
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, Entity};
use arkiv_interfaces::entity_records;
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use arkiv_interfaces::query::Query as ArkivQuery;
use arkiv_interfaces::store::{Filter, Page, Query, Store, StoreError};
use arkiv_rpc_types::method::MAX_PAGE_SIZE;
use tempfile::TempDir;

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
        genesis: Genesis::new(HashAlgorithm::Keccak256, limits, record_limits, None),
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

/// `text` as the store filter `arkiv_query` runs it as.
fn filter(text: &str) -> Filter {
    Filter::from(&arkiv_query::parse(text).unwrap())
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

/// A page is `offset` matches in, `limit` long, over GolemDB's own order: the
/// order the entities were created in, not their keys' order, and they are
/// written out of key order here so the two differ. The pages partition the
/// matches: walking a limit at a time yields every match once, in that order,
/// and an offset past the last one is an empty page, not an error.
/// `total_matched` is the whole match set, independent of the page.
///
/// GolemDB caps a page at its own query limit; over that, a page is
/// `LimitExceeded` rather than a silent trim.
#[test]
fn a_query_pages_by_limit_and_offset() {
    let dir = TempDir::new().unwrap();
    let created = [
        entity(3, "red"),
        entity(1, "red"),
        entity(5, "red"),
        entity(2, "red"),
        entity(4, "red"),
    ];
    let store = store_with(&dir, &created);

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

    // A limit that holds everything, in creation order, and one that holds
    // nothing left to hold.
    assert_eq!(keys(0, 10), [3, 1, 5, 2, 4]);
    assert!(keys(5, 10).is_empty());
    assert!(keys(9, 10).is_empty());

    // Successive pages partition the matches, in creation order.
    assert_eq!(keys(0, 2), [3, 1]);
    assert_eq!(keys(2, 2), [5, 2]);
    assert_eq!(keys(4, 2), [4]);
    assert!(keys(6, 2).is_empty());

    // An offset with no limit left over, and a limit of one.
    assert_eq!(keys(1, 3), [1, 5, 2]);
    assert_eq!(keys(0, 1), [3]);
    assert_eq!(keys(4, 1), [4]);

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

/// The largest page `arkiv_query` serves is one GolemDB runs: the RPC's
/// ceiling is GolemDB's own query limit, so a full page is never refused.
#[test]
fn the_largest_page_arkiv_query_serves_runs() {
    let dir = TempDir::new().unwrap();
    let store = store_with(&dir, &[entity(1, "red")]);
    let query = Query {
        filter: filter("team = str('red')"),
        page: Page {
            offset: 0,
            limit: MAX_PAGE_SIZE,
        },
        ..Query::default()
    };
    assert_eq!(
        store
            .query(None, &query, None)
            .map(|page| page.into_value().records.len()),
        Ok(1)
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

/// GolemDB stores a `key` as the 32 fixed bytes a `bytes32` is, so it cannot
/// tell the two apart. `typeof(attr) = key` is not implemented rather than
/// also answering the `bytes32` holders; `typeof(attr) = bytes32` finds both,
/// as a `key` attribute reads back from GolemDB as a `bytes32` anyway.
#[test]
fn typeof_key_is_not_implemented_and_typeof_bytes32_finds_both() {
    let dir = TempDir::new().unwrap();
    let store = store_with(
        &dir,
        &[
            entity_with(
                1,
                vec![Attribute::new(
                    "parent",
                    AttributeValue::EntityKey([0xaa; 32]),
                )],
            ),
            entity_with(
                2,
                vec![Attribute::new(
                    "parent",
                    AttributeValue::Bytes32([0xbb; 32]),
                )],
            ),
        ],
    );

    let typeof_key = Query {
        filter: filter("typeof(parent) = key"),
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };
    assert_eq!(
        store
            .query(None, &typeof_key, None)
            .map(|page| page.into_value()),
        Err(StoreError::NotImplemented)
    );
    assert_eq!(
        store
            .count(None, &typeof_key.filter, None)
            .map(|count| count.into_value()),
        Err(StoreError::NotImplemented)
    );
    assert_eq!(matched(&store, filter("typeof(parent) = bytes32")), [1, 2]);
}

/// A range follows its type's order for the types that are not numbers:
/// `false < true`, and unsigned byte order for a `str`, a `bytes32` and an
/// `addr`, the system `$owner` and `$key` included.
#[test]
fn a_range_follows_the_order_of_every_type() {
    /// `N` bytes starting with `first`: `0x80` must sort above `0x7f`, which
    /// a sign bias would invert.
    fn starting_with<const N: usize>(first: u8) -> [u8; N] {
        let mut bytes = [0u8; N];
        bytes[0] = first;
        bytes
    }
    let hex = |first: u8, len: usize| format!("0x{first:02x}{}", "00".repeat(len - 1));

    let entities: Vec<Entity> = [
        (1, false, "a", 0x00),
        (2, true, "ab", 0x7f),
        (3, true, "b", 0x80),
    ]
    .into_iter()
    .map(|(n, flag, team, first)| {
        entity_with(
            n,
            vec![
                Attribute::new("flag", AttributeValue::Bool(flag)),
                Attribute::new("hash", AttributeValue::Bytes32(starting_with(first))),
                Attribute::new("team", AttributeValue::Str(team.into())),
                Attribute::new("who", AttributeValue::EthereumAddress(starting_with(first))),
            ],
        )
    })
    .collect();
    let dir = TempDir::new().unwrap();
    let store = store_with(&dir, &entities);

    for (query, expected) in [
        ("flag > false".to_string(), [2, 3]),
        // A prefix sorts before the strings it starts.
        ("team > str('a')".to_string(), [2, 3]),
        ("team <= str('ab')".to_string(), [1, 2]),
        (format!("hash >= bytes32({})", hex(0x7f, 32)), [2, 3]),
        (format!("hash < bytes32({})", hex(0x80, 32)), [1, 2]),
        (format!("who >= addr({})", hex(0x7f, 20)), [2, 3]),
        (format!("$owner > addr(0x{})", "01".repeat(20)), [2, 3]),
        (format!("$key <= key(0x{})", "02".repeat(32)), [1, 2]),
    ] {
        assert_eq!(matched(&store, filter(&query)), expected, "{query}");
    }
}

/// `BETWEEN` includes both bounds, in the type's order, and matches nothing
/// when its low bound is above its high one. It counts as it queries, and
/// combines with the live-entity bound the RPC adds.
#[test]
fn a_between_includes_both_bounds() {
    let entities: Vec<Entity> = [(1, -5, "a"), (2, 0, "ab"), (3, 5, "b")]
        .into_iter()
        .map(|(n, level, team)| {
            entity_with(
                n,
                vec![
                    Attribute::new("level", AttributeValue::Int(level)),
                    Attribute::new("team", AttributeValue::Str(team.into())),
                ],
            )
        })
        .collect();
    let dir = TempDir::new().unwrap();
    let store = store_with(&dir, &entities);

    for (query, expected) in [
        ("level BETWEEN -5 AND 0", vec![1, 2]),
        ("level BETWEEN 0 AND 0", vec![2]),
        ("level BETWEEN -6 AND 6", vec![1, 2, 3]),
        ("level BETWEEN 1 AND 4", vec![]),
        ("level BETWEEN 5 AND -5", vec![]),
        ("team BETWEEN str('a') AND str('ab')", vec![1, 2]),
        (
            "level BETWEEN -5 AND 5 AND $expiresAt > u64(999)",
            vec![1, 2, 3],
        ),
        ("level BETWEEN 0 AND 5 OR team = str('a')", vec![1, 2, 3]),
    ] {
        assert_eq!(matched(&store, filter(query)), expected, "{query}");
        let count = store
            .count(None, &filter(query), None)
            .expect("GolemDB counts the query")
            .into_value();
        assert_eq!(count, expected.len() as u64, "{query}");
    }
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
        count("(team = str('red') OR team = str('blue')) AND $expiresAt > u64(500)"),
        Ok(3),
        "an AND of an OR, answered as written"
    );
    assert_eq!(
        count("NOT (team = str('red'))"),
        Err(StoreError::NotImplemented)
    );
}

/// `*` is every entity, which GolemDB answers as the records carrying `$key`:
/// alone, and bounded to live entities the way `arkiv_query` bounds it.
#[test]
fn star_matches_every_entity() {
    let dir = TempDir::new().unwrap();
    let store = store_with(
        &dir,
        &[entity(1, "red"), entity(2, "blue"), entity(3, "red")],
    );
    let live_at = |block: u64| {
        let bound = arkiv_query::parse(&format!("$expiresAt > u64({block})")).unwrap();
        Filter::from(&ArkivQuery::And(vec![ArkivQuery::All, bound]))
    };
    let count = |filter: &Filter| store.count(None, filter, None).map(|n| n.into_value());

    assert_eq!(count(&filter("*")), Ok(3));
    assert_eq!(count(&live_at(500)), Ok(3));
    assert_eq!(count(&live_at(1_000)), Ok(0), "every entity has expired");
    assert_eq!(matched(&store, live_at(500)), [1, 2, 3]);
}
