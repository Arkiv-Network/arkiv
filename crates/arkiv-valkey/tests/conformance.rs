//! `ValkeyStore` against the shared store contract.
//!
//! These need a real server, so they are `#[ignore]` by default and run in CI
//! against a Valkey service container:
//!
//! ```sh
//! VALKEY_URL=redis://127.0.0.1:6379 cargo test -p arkiv-valkey -- --ignored
//! ```
//!
//! Gating on a live server rather than a fake is deliberate. A fake would only
//! prove that our model of Valkey is self-consistent — which
//! [`MemStore`](arkiv_interfaces::store::reference::MemStore) already does,
//! more cheaply and without a network. The whole point of this crate is the
//! behaviour of the real thing.

use arkiv_interfaces::store::conformance;
use arkiv_valkey::ValkeyStore;

/// Where the server is, or a clear explanation of why we cannot proceed.
///
/// Panics rather than silently passing: a green test that never connected is
/// worse than no test.
fn url() -> String {
    std::env::var("VALKEY_URL").expect(
        "these tests need a Valkey server: set VALKEY_URL \
         (for example redis://127.0.0.1:6379)",
    )
}

/// A store on a namespace nothing else is using, so assertions that each build
/// a fresh store cannot see one another's writes.
fn new_store() -> ValkeyStore {
    ValkeyStore::ephemeral(&url()).expect("connect to the valkey server")
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn store_conformance() {
    conformance::run_all(&new_store);
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn store_ext_conformance() {
    conformance::run_all_ext(&new_store);
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn segment_conformance() {
    conformance::run_all_segments(&new_store);
}

/// Appended rows outlive the connection too, addressable by both ordinal and
/// key — the half of the segment contract `MemStore` cannot demonstrate.
#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn segment_rows_survive_reconnection() {
    use arkiv_interfaces::store::{SegmentAddress, SegmentKey, Store};

    let namespace = format!("arkiv-test:segments:{}", std::process::id());
    let key = SegmentKey([9u8; 32]);
    let row = vec![b"a header".to_vec()];

    let committed = {
        let store = ValkeyStore::connect(&url(), &namespace).expect("connect");
        let branch = store.begin(None).expect("begin");
        store.seal(branch).expect("seal");
        store
            .segment_append(branch, "blocks", Some(key), row.clone())
            .expect("append");
        store.commit(branch).expect("commit")
    };

    let reopened = ValkeyStore::connect(&url(), &namespace).expect("reconnect");
    assert_eq!(
        reopened.segment_get("blocks", SegmentAddress::Key(key)),
        Ok(Some(row.clone())),
        "the row is still reachable by its key",
    );
    assert_eq!(
        reopened.segment_get("blocks", SegmentAddress::Ordinal(0)),
        Ok(Some(row)),
        "and by its ordinal",
    );
    assert_eq!(
        reopened.segment_range_of("blocks", committed),
        Ok(0..1),
        "and the commit still knows what it appended",
    );
    reopened.drop_namespace().expect("clean up");
}

/// Committed state outlives the connection that wrote it, and reconnecting to
/// the same namespace resumes from it — the property that separates this from
/// an in-memory store.
#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn committed_state_survives_reconnection() {
    use arkiv_interfaces::store::{Cell, ReadTarget, Store, TypeId};

    let namespace = format!("arkiv-test:reconnect:{}", std::process::id());
    let key = arkiv_interfaces::store::RecordKey([7u8; 32]);

    let committed = {
        let store = ValkeyStore::connect(&url(), &namespace).expect("connect");
        let branch = store.begin(None).expect("begin");
        store
            .create(
                branch,
                key,
                vec![("n".to_owned(), Cell::attribute(TypeId::U64, vec![0; 8]))],
                None,
            )
            .expect("create");
        store.commit(branch).expect("commit")
    };

    let reopened = ValkeyStore::connect(&url(), &namespace).expect("reconnect");
    assert_eq!(
        reopened.head(),
        committed,
        "the head survived the reconnect"
    );
    assert!(
        reopened
            .get(ReadTarget::Commit(committed), key, None, None)
            .expect("get")
            .into_value()
            .is_some(),
        "and so did the record"
    );
    reopened.drop_namespace().expect("clean up");
}

/// A handle whose mirror is already loaded still answers for commits another
/// handle made since, and still answers for the commits before them -- the
/// two ways an in-process mirror could go stale.
#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn queries_follow_commits_made_by_another_handle() {
    use arkiv_interfaces::store::{
        AndGroup, Cell, CompareOp, Filter, Page, Predicate, Query, ReadTarget, RecordKey, Store,
        TypeId,
    };

    let namespace = format!("arkiv-test:mirror:{}", std::process::id());
    let writer = ValkeyStore::connect(&url(), &namespace).expect("connect writer");
    let reader = ValkeyStore::connect(&url(), &namespace).expect("connect reader");

    let colour = |value: &str| {
        vec![(
            "colour".to_owned(),
            Cell::attribute(TypeId::STR, value.as_bytes().to_vec()),
        )]
    };
    let red = || {
        Filter(vec![AndGroup(vec![Predicate {
            cell: "colour".to_owned(),
            op: CompareOp::Eq,
            type_id: TypeId::STR,
            value: b"red".to_vec(),
            negated: false,
        }])])
    };
    let keys_of = |at| {
        reader
            .query(
                Some(at),
                &Query {
                    filter: red(),
                    page: Page {
                        offset: 0,
                        limit: 10,
                    },
                    ..Query::default()
                },
                None,
            )
            .expect("query")
            .into_value()
            .records
            .into_iter()
            .map(|record| record.key.0[0])
            .collect::<Vec<_>>()
    };

    // Commit 1: two red records. The reader loads its mirror here.
    let branch = writer.begin(None).expect("begin");
    for byte in [1u8, 2] {
        writer
            .create(branch, RecordKey([byte; 32]), colour("red"), None)
            .expect("create");
    }
    let first = writer.commit(branch).expect("commit 1");
    assert_eq!(keys_of(first), [1, 2]);

    // Commit 2, by the writer alone: one record turns blue, a third arrives.
    let branch = writer.begin(None).expect("begin");
    writer
        .patch(
            branch,
            RecordKey([1; 32]),
            None,
            vec![(
                "colour".to_owned(),
                arkiv_interfaces::store::CellChange::Set(Cell::attribute(
                    TypeId::STR,
                    b"blue".to_vec(),
                )),
            )],
            None,
        )
        .expect("patch");
    writer
        .create(branch, RecordKey([3; 32]), colour("red"), None)
        .expect("create");
    let second = writer.commit(branch).expect("commit 2");

    assert_eq!(keys_of(second), [2, 3], "the reader rolled forward");
    assert_eq!(keys_of(first), [1, 2], "and still answers for the past");
    assert_eq!(
        reader
            .count(Some(second), &red(), None)
            .expect("count")
            .into_value(),
        2
    );
    assert!(
        reader
            .get(ReadTarget::Commit(first), RecordKey([3; 32]), None, None)
            .expect("historical get")
            .into_value()
            .is_none(),
        "a record made later is absent from an earlier commit",
    );
    writer.drop_namespace().expect("clean up");
}
