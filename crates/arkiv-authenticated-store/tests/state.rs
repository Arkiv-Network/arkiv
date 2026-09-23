use arkiv_authenticated_store::{EMPTY_ROOT, MdbxStore, MemoryStore, State};
use arkiv_interfaces::{
    entity::{Attribute, AttributeType, AttributeValue, Entity},
    query::{AnnotKey, PageParams, Query},
};
use eyre::Result;
use std::ops::Bound::{Excluded, Included, Unbounded};

fn entity(n: u8, age: i32, name: &str) -> Entity {
    // Deliberately share the first 20 bytes: the new layout must preserve all 32.
    let mut key = [42; 32];
    key[31] = n;
    Entity {
        key,
        creator: [1; 20],
        owner: [2; 20],
        expires_at: 100,
        payload: vec![n; 16],
        content_type: b"text/plain".to_vec(),
        attributes: vec![
            Attribute::new(b"age".to_vec(), AttributeValue::Int(age)),
            Attribute::new(b"name".to_vec(), AttributeValue::Str(name.into())),
        ],
        ..Default::default()
    }
}

#[test]
fn typed_ranges_boolean_queries_and_latest_first_pages() -> Result<()> {
    let mut state = State::open(MemoryStore::default(), EMPTY_ROOT)?;
    for e in [
        entity(1, -5, "alice"),
        entity(2, 30, "bob"),
        entity(3, 40, "carol"),
        entity(4, 30, "alicia"),
    ] {
        state.put_entity(&e)?;
        assert_eq!(state.entity(e.key)?, Some(e));
    }
    let (hits, stats) = state.range(
        b"age",
        AttributeType::Int,
        Included(&AttributeValue::Int(30)),
        Excluded(&AttributeValue::Int(40)),
    )?;
    assert_eq!(hits.iter().collect::<Vec<_>>(), [1, 3]);
    assert_eq!(stats.values_read, 1, "one bitmap covers both equal values");
    let (negative, _) = state.range(
        b"age",
        AttributeType::Int,
        Unbounded,
        Excluded(&AttributeValue::Int(0)),
    )?;
    assert_eq!(negative.iter().collect::<Vec<_>>(), [0]);
    assert!(state.equal(b"age", &AttributeValue::U64(30))?.is_empty());
    let query = Query::And(
        Box::new(Query::Gte {
            key: AnnotKey::User("age".into()),
            value: AttributeValue::Int(30),
        }),
        Box::new(Query::StartsWith {
            key: AnnotKey::User("name".into()),
            value: AttributeValue::Str("ali".into()),
        }),
    );
    assert_eq!(state.evaluate(&query)?.iter().collect::<Vec<_>>(), [3]);
    assert_eq!(
        state
            .evaluate(&Query::Not(Box::new(query)))?
            .iter()
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    let first = state.page(
        &Query::All,
        PageParams {
            page_size: 2,
            cursor: None,
        },
    )?;
    assert_eq!(first.keys, [entity(4, 0, "").key, entity(3, 0, "").key]);
    let second = state.page(
        &Query::All,
        PageParams {
            page_size: 2,
            cursor: first.next_cursor,
        },
    )?;
    assert_eq!(second.keys, [entity(2, 0, "").key, entity(1, 0, "").key]);
    assert_eq!(second.next_cursor, None);
    Ok(())
}

#[test]
fn long_and_overlapping_string_prefixes_survive_deletion() -> Result<()> {
    let mut state = State::open(MemoryStore::default(), EMPTY_ROOT)?;
    let prefix = "x".repeat(70);
    let a = entity(1, 1, &prefix);
    let b = entity(2, 2, &format!("{prefix}suffix"));
    let c = entity(3, 3, "elsewhere");
    for e in [&a, &b, &c] {
        state.put_entity(e)?;
    }
    let (hits, stats) = state.prefix(b"name", &prefix)?;
    assert_eq!(hits.len(), 2);
    assert_eq!(stats.values_read, 2);
    state.remove_entity(a.key)?;
    assert_eq!(state.keys(&state.prefix(b"name", &prefix)?.0)?, [b.key]);
    state.remove_entity(b.key)?;
    assert!(state.prefix(b"name", &prefix)?.0.is_empty());
    assert_eq!(state.keys(&state.prefix(b"name", "")?.0)?, [c.key]);
    Ok(())
}

#[test]
fn updates_remove_old_buckets_and_duplicate_attributes_are_sets() -> Result<()> {
    let mut state = State::open(MemoryStore::default(), EMPTY_ROOT)?;
    let mut a = entity(1, 20, "alice");
    a.attributes.push(a.attributes[0].clone());
    state.put_entity(&a)?;
    let root = state.root();
    state.put_entity(&a)?;
    assert_eq!(state.root(), root, "idempotent write");
    let b = entity(2, 20, "bob");
    state.put_entity(&b)?;
    let changed = entity(1, 30, "changed");
    state.put_entity(&changed)?;
    assert_eq!(
        state.keys(&state.equal(b"age", &AttributeValue::Int(20))?)?,
        [b.key]
    );
    assert!(
        state
            .equal(b"name", &AttributeValue::Str("alice".into()))?
            .is_empty()
    );
    state.remove_entity(b.key)?;
    assert!(state.equal(b"age", &AttributeValue::Int(20))?.is_empty());
    assert_eq!(state.keys(&state.evaluate(&Query::All)?)?, [a.key]);
    Ok(())
}

#[test]
fn rollback_covers_entity_indexes_id_allocation_and_creation_nonce() -> Result<()> {
    let mut state = State::open(MemoryStore::default(), EMPTY_ROOT)?;
    let first = entity(1, 30, "alice");
    let result: Result<()> = state.transaction(|state| {
        state.put_entity(&first)?;
        state.increment_creation_nonce([1; 20])?;
        eyre::bail!("failed operation");
    });
    assert!(result.is_err());
    assert_eq!(state.root(), EMPTY_ROOT);
    assert_eq!(state.creation_nonce([1; 20])?, 0);
    assert!(state.entity(first.key)?.is_none());
    assert!(state.evaluate(&Query::All)?.is_empty());
    state.put_entity(&entity(2, 40, "bob"))?;
    assert_eq!(state.evaluate(&Query::All)?.iter().collect::<Vec<_>>(), [0]);
    Ok(())
}

#[test]
fn disk_reopen_and_reorg_restore_every_namespace() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = MdbxStore::open(dir.path())?;
    let mut state = State::open(store.clone(), EMPTY_ROOT)?;
    let a = entity(1, 10, "a");
    state.put_entity(&a)?;
    state.increment_creation_nonce(a.creator)?;
    let parent = state.persist()?;
    state.remove_entity(a.key)?;
    state.put_entity(&entity(2, 40, "b"))?;
    state.increment_creation_nonce(a.creator)?;
    let child = state.persist()?;
    drop(state);
    drop(store);
    let store = MdbxStore::open(dir.path())?;
    let parent = State::open(store.clone(), parent)?;
    let child = State::open(store, child)?;
    assert_eq!(parent.entity(a.key)?, Some(a.clone()));
    assert!(child.entity(a.key)?.is_none());
    assert_eq!(parent.creation_nonce(a.creator)?, 1);
    assert_eq!(child.creation_nonce(a.creator)?, 2);
    assert_eq!(
        parent.evaluate(&Query::All)?.iter().collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(child.evaluate(&Query::All)?.iter().collect::<Vec<_>>(), [1]);
    Ok(())
}

#[test]
fn portable_snapshot_validates_all_records_before_import() -> Result<()> {
    use arkiv_authenticated_store::{RecordStore, Snapshot};
    let mut state = State::open(MemoryStore::default(), EMPTY_ROOT)?;
    state.put_entity(&entity(1, 30, "alice"))?;
    state.increment_creation_nonce([2; 20])?;
    let snapshot = state.snapshot()?;
    let target = MemoryStore::default();
    snapshot.import(target.clone())?;
    let copy = State::open(target, snapshot.root)?;
    assert_eq!(
        copy.entity(entity(1, 0, "").key)?,
        state.entity(entity(1, 0, "").key)?
    );
    assert_eq!(copy.creation_nonce([2; 20])?, 1);

    let mut corrupt = snapshot.clone();
    let hash = *corrupt.records.keys().next().unwrap();
    corrupt.records.insert(hash, vec![0xff].into());
    let clean = MemoryStore::default();
    assert!(corrupt.import(clean.clone()).is_err());
    assert!(
        clean.get(snapshot.root)?.is_none(),
        "failed imports are atomic"
    );
    let mut missing = snapshot.clone();
    missing.records.remove(&hash);
    assert!(missing.import(clean.clone()).is_err());
    let empty = Snapshot {
        root: EMPTY_ROOT,
        records: Default::default(),
    };
    empty.import(clean)?;
    Ok(())
}

#[test]
fn expiration_selection_respects_budget_and_fork() -> Result<()> {
    let records = MemoryStore::default();
    let mut original = State::open(records.clone(), EMPTY_ROOT)?;
    let e = entity(1, 30, "alice");
    original.put_entity(&e)?;
    let root = original.persist()?;
    let cost = arkiv_interfaces::gas::purge_cost(e.attributes.len());
    assert!(original.expired(99, 10, u64::MAX)?.is_empty());
    assert!(original.expired(100, 10, cost - 1)?.is_empty());
    assert_eq!(original.expired(100, 10, cost)?.len(), 1);
    let mut branch = State::open(records, root)?;
    branch.put_entity(&Entity {
        expires_at: 200,
        ..e
    })?;
    branch.persist()?;
    assert!(branch.expired(100, 10, u64::MAX)?.is_empty());
    assert_eq!(original.expired(100, 10, u64::MAX)?.len(), 1);
    Ok(())
}
