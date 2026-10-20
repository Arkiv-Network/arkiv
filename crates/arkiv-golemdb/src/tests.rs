use super::*;
use arkiv_interfaces::store::{AndGroup, CellKind, CompareOp, Page, Predicate, Sort, TypeId};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn config() -> GolemDbConfig {
    GolemDbConfig {
        hash_function: HashAlgorithm::Keccak256,
        cell_limits: CellLimits {
            max_cell_name_len: 32,
            max_str_len: 128,
            max_bytes_len: 128 * 1024,
        },
    }
}

fn open(dir: &TempDir) -> GolemDbStore {
    GolemDbStore::open(dir.path(), &config()).unwrap()
}

fn cell(kind: CellKind, type_id: TypeId, value: &[u8]) -> Cell {
    Cell {
        kind,
        type_id,
        value: value.to_vec(),
    }
}

fn attribute(type_id: TypeId, value: &[u8]) -> Cell {
    cell(CellKind::Attribute, type_id, value)
}

fn read(store: &GolemDbStore, target: ReadTarget, key: u8) -> Result<Record, StoreError> {
    Ok(store
        .get(target, RecordKey([key; 32]), None, None)?
        .into_value()
        .expect("record exists"))
}

/// Commit one record, keyed by `n`, on a branch over the head.
fn commit_record(store: &GolemDbStore, n: u8) -> CommitId {
    let branch = store.begin(Some(store.head())).unwrap();
    let cells = vec![(
        "n".into(),
        attribute(TypeId::U64, &u64::from(n).to_be_bytes()),
    )];
    store
        .create(branch, RecordKey([n; 32]), cells, None)
        .unwrap();
    store.commit(branch).unwrap()
}

#[test]
fn every_type_round_trips() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let mut cells = vec![
        ("$key".into(), attribute(TypeId::KEY, &[7; 32])),
        ("b".into(), attribute(TypeId::BOOL, &[1])),
        ("i".into(), attribute(TypeId::I32, &(-5i32).to_be_bytes())),
        ("u".into(), attribute(TypeId::U256, &[0xab; 32])),
        ("d".into(), attribute(TypeId::DEC, &[0xff; 32])),
        ("h".into(), attribute(TypeId::BYTES32, &[9; 32])),
        ("p".into(), cell(CellKind::Field, TypeId::BYTES, b"payload")),
        ("s".into(), attribute(TypeId::STR, b"hello")),
        ("n".into(), attribute(TypeId::U64, &42u64.to_be_bytes())),
        ("a".into(), attribute(TypeId::ADDR, &[3; 20])),
    ];
    let branch = store.begin(None).unwrap();
    store
        .create(branch, RecordKey([1; 32]), cells.clone(), None)
        .unwrap();
    let commit = store.commit(branch).unwrap();

    let mut read = read(&store, ReadTarget::Commit(commit), 1).unwrap().cells;
    read.sort_by(|a, b| a.0.cmp(&b.0));
    cells.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(read, cells);
}

/// GolemDB has no key type: only the entity's own `$key` comes back as `KEY`.
#[test]
fn key_cells_keep_their_type_and_answer_equality() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    // A high first byte, so the sign-bit flip has something to undo.
    let reference = [0xf5; 32];
    let cells = vec![
        ("ref".into(), attribute(TypeId::KEY, &reference)),
        ("hash".into(), attribute(TypeId::BYTES32, &reference)),
    ];
    store
        .create(branch, RecordKey([1; 32]), cells, None)
        .unwrap();
    let record = read(&store, ReadTarget::Branch(branch), 1).unwrap();
    assert_eq!(
        record.cell("ref").unwrap(),
        &attribute(TypeId::KEY, &reference)
    );
    assert_eq!(
        record.cell("hash").unwrap(),
        &attribute(TypeId::BYTES32, &reference)
    );
    store.commit(branch).unwrap();

    let by = |type_id| {
        let filter = Filter(vec![AndGroup(vec![Predicate {
            cell: "ref".into(),
            op: CompareOp::Eq,
            type_id,
            value: reference.to_vec(),
            negated: false,
        }])]);
        store.count(None, &filter, None).unwrap().into_value()
    };
    assert_eq!(by(TypeId::KEY), 1);
    // A cell of another type under the name is absent, as for any type.
    assert_eq!(by(TypeId::BYTES32), 0);
}

/// Arkiv's `$creationFlags` is a `BOOL` holding up to 3; GolemDB takes 0 or 1.
#[test]
fn a_bool_above_one_is_refused() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    let cells = vec![("f".into(), cell(CellKind::Field, TypeId::BOOL, &[2]))];
    assert_eq!(
        store.create(branch, RecordKey([1; 32]), cells, None),
        Err(StoreError::InvalidArgument)
    );
}

/// Once another branch commits, every call on an older one is refused, reads
/// included.
#[test]
fn a_stale_branch_is_dead() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let winner = store.begin(None).unwrap();
    let loser = store.begin(None).unwrap();
    store.commit(winner).unwrap();

    let stale = Err(StoreError::HandleInvalid);
    assert_eq!(
        read(&store, ReadTarget::Branch(loser), 1).map(|_| ()),
        stale
    );
    assert_eq!(
        store
            .create(loser, RecordKey([1; 32]), vec![], None)
            .map(|_| ()),
        stale
    );
    assert_eq!(store.commit(loser).map(|_| ()), stale);
    assert_eq!(store.head(), CommitId(1));
}

#[test]
fn no_branch_over_an_older_commit_and_its_reads_answer_from_the_head() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let first = commit_record(&store, 1);
    commit_record(&store, 2);

    assert_eq!(store.begin(Some(first)), Err(StoreError::HandleInvalid));
    // Record 2 did not exist at `first`: the read is the head's.
    assert!(read(&store, ReadTarget::Commit(first), 2).is_ok());
}

#[test]
fn a_commit_publishes_the_sealed_root() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    let sealed = store.seal(branch).unwrap();
    assert_eq!(store.commit(branch), Ok(sealed.commit_nr));
    assert_eq!(store.head(), sealed.commit_nr);
}

#[test]
fn reopening_keeps_head_and_records_but_not_another_config() {
    let dir = TempDir::new().unwrap();
    let head = commit_record(&open(&dir), 1);

    let store = open(&dir);
    assert_eq!(store.head(), head);
    assert!(read(&store, ReadTarget::Commit(head), 1).is_ok());
    drop(store);

    let mut other = config();
    other.cell_limits.max_str_len += 1;
    assert!(GolemDbStore::open(dir.path(), &other).is_err());
}

/// Create record `n`, patch record `n - 1` and commit, as a block would.
fn commit_block(store: &GolemDbStore, n: u8) -> SealedCommit {
    let branch = store.begin(Some(store.head())).unwrap();
    let value = |n: u8| attribute(TypeId::U64, &u64::from(n).to_be_bytes());
    store
        .create(
            branch,
            RecordKey([n; 32]),
            vec![("n".into(), value(n))],
            None,
        )
        .unwrap();
    if n > 1 {
        let changes = vec![("n".into(), CellChange::Set(value(0)))];
        store
            .patch(branch, RecordKey([n - 1; 32]), None, changes, None)
            .unwrap();
    }
    let sealed = store.seal(branch).unwrap();
    store.commit(branch).unwrap();
    sealed
}

/// Every node must seal the same roots for the same writes, whether or not it
/// restarted in between.
#[test]
fn roots_do_not_depend_on_a_restart() {
    let (steady_dir, restarted_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let steady = open(&steady_dir);
    let mut state_roots = Vec::new();
    for n in 1..=3 {
        let restarted = open(&restarted_dir);
        let sealed = commit_block(&steady, n);
        assert_eq!(commit_block(&restarted, n), sealed, "commit {n}");
        state_roots.push(sealed.state_root);
    }
    state_roots.dedup();
    assert_eq!(state_roots.len(), 3, "every commit changes the root");
}

/// Set in the child process of `a_killed_process_keeps_only_its_commits`: the
/// directory it opens.
const CHILD_DIR: &str = "ARKIV_GOLEMDB_TEST_CHILD_DIR";

/// A child process commits record 1, writes record 2 on a branch it never
/// commits, and is killed while it holds the store, so no cleanup runs.
#[test]
fn a_killed_process_keeps_only_its_commits() {
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        let store = GolemDbStore::open(dir.as_ref(), &config()).unwrap();
        commit_record(&store, 1);
        let branch = store.begin(None).unwrap();
        let cells = vec![("n".into(), attribute(TypeId::U64, &2u64.to_be_bytes()))];
        store
            .create(branch, RecordKey([2; 32]), cells, None)
            .unwrap();
        eprintln!("ready");
        loop {
            std::thread::park();
        }
    }

    let dir = TempDir::new().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::a_killed_process_keeps_only_its_commits",
            "--nocapture",
        ])
        .env(CHILD_DIR, dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready = BufReader::new(child.stderr.take().unwrap())
        .lines()
        .next()
        .and_then(Result::ok);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(ready.as_deref(), Some("ready"));

    let store = open(&dir);
    assert_eq!(store.head(), CommitId(1));
    assert!(read(&store, ReadTarget::Commit(CommitId(1)), 1).is_ok());
    let uncommitted = store
        .get(
            ReadTarget::Commit(CommitId(1)),
            RecordKey([2; 32]),
            None,
            None,
        )
        .unwrap();
    assert!(uncommitted.into_value().is_none());
}

/// A round trip cannot catch a wrong sign flip, because the flip undoes
/// itself. These bytes go under the state root, so pin them to GolemDB's own
/// encoding.
#[test]
fn signed_values_encode_as_golemdb_does() {
    let attribute_kind =
        |value: golemdb::CellValue| value.with_kind(golemdb::CellKind::Attribute).unwrap();
    let int = convert::golemdb_cell(&attribute(TypeId::I32, &(-5i32).to_be_bytes()));
    assert_eq!(int, Ok(attribute_kind(golemdb::CellValue::from_i32(-5))));

    let mut minus_five = [0xff; 32];
    minus_five[31] = 0xfb;
    let dec = convert::golemdb_cell(&attribute(TypeId::DEC, &minus_five));
    assert_eq!(
        dec,
        Ok(attribute_kind(golemdb::CellValue::from_dec256_unscaled_be(
            minus_five
        )))
    );
}

/// `begin` opens the first frame; one rollback undoes it, the next is refused.
#[test]
fn rolling_back_past_the_first_frame_is_handle_invalid() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    store.rollback(branch).unwrap();
    assert_eq!(store.rollback(branch), Err(StoreError::HandleInvalid));
}

fn tagged(
    store: &GolemDbStore,
    n: u8,
    rows: &[(&str, Option<SegmentKey>, SegmentRow)],
) -> CommitId {
    let branch = store.begin(None).unwrap();
    let cells = vec![(
        "n".into(),
        attribute(TypeId::U64, &u64::from(n).to_be_bytes()),
    )];
    store
        .create(branch, RecordKey([n; 32]), cells, None)
        .unwrap();
    store.seal(branch).unwrap();
    store.segment_append_many(branch, rows).unwrap();
    store.commit_tagged(branch, [n; 32]).unwrap()
}

#[test]
fn commit_tags_resolve_both_ways_and_keep_the_root() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let untagged = commit_record(&store, 1);
    let branch = store.begin(None).unwrap();
    let sealed = store.seal(branch).unwrap();
    let commit = store.commit_tagged(branch, [0xaa; 32]).unwrap();

    assert_eq!(commit, sealed.commit_nr);
    assert_eq!(store.commit_by_tag([0xaa; 32]), Ok(Some(commit)));
    assert_eq!(store.commit_by_tag([0xbb; 32]), Ok(None));
    assert_eq!(store.tag_of(commit), Ok(Some([0xaa; 32])));
    assert_eq!(store.tag_of(untagged), Ok(None));
    assert_eq!(store.commit_hash(commit), Ok(sealed.state_root));
    assert_eq!(store.commit_hash(untagged), Err(StoreError::NotFound));

    // A tag names one commit.
    let branch = store.begin(None).unwrap();
    assert_eq!(
        store.commit_tagged(branch, [0xaa; 32]),
        Err(StoreError::AlreadyExists)
    );
}

#[test]
fn segments_group_rows_by_commit_and_resolve_names() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let tx = |n: u8| (vec![n; 40], vec![n; 20], vec![n; 32]);
    let row = |n: u8| {
        let (a, b, c) = tx(n);
        vec![a, b, c]
    };
    let first = tagged(
        &store,
        1,
        &[
            ("transactions", Some(SegmentKey([1; 32])), row(1)),
            ("transactions", Some(SegmentKey([2; 32])), row(2)),
            ("receipts", Some(SegmentKey([1; 32])), vec![vec![9]]),
        ],
    );
    let empty = tagged(&store, 2, &[]);
    let third = tagged(
        &store,
        3,
        &[("transactions", Some(SegmentKey([3; 32])), row(3))],
    );

    assert_eq!(store.segment_range_of("transactions", first), Ok(0..2));
    assert_eq!(store.segment_range_of("transactions", empty), Ok(2..2));
    assert_eq!(store.segment_range_of("transactions", third), Ok(2..3));
    assert_eq!(store.segment_range_of("blocks", third), Ok(0..0));
    assert_eq!(store.segment_range_of("never_declared", third), Ok(0..0));
    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Key(SegmentKey([2; 32]))),
        Ok(Some(row(2)))
    );
    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Ordinal(2)),
        Ok(Some(row(3)))
    );
    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Ordinal(3)),
        Ok(None)
    );
    assert_eq!(
        store.segment_ordinal_of("transactions", SegmentKey([3; 32])),
        Ok(Some(2))
    );
    assert_eq!(
        store.segment_ordinal_of("transactions", SegmentKey([7; 32])),
        Ok(None)
    );
    assert_eq!(
        store.segment_rows_of("transactions", first),
        Ok(vec![row(1), row(2)])
    );
    assert_eq!(store.segment_rows_of("receipts", empty), Ok(vec![]));

    // The tags segment is the adapter's, not a host's.
    let branch = store.begin(None).unwrap();
    assert_eq!(
        store.segment_append(branch, "commit_tags", None, vec![vec![]]),
        Err(StoreError::InvalidArgument)
    );
}

fn predicate(cell: &str, op: CompareOp, type_id: TypeId, value: &[u8], negated: bool) -> Predicate {
    Predicate {
        cell: cell.into(),
        op,
        type_id,
        value: value.to_vec(),
        negated,
    }
}

#[test]
fn queries_match_page_and_count() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    for n in 1..=6u8 {
        let kind = if n % 2 == 0 { "even" } else { "odd" };
        let cells = vec![
            (
                "n".into(),
                attribute(TypeId::U64, &u64::from(n).to_be_bytes()),
            ),
            ("kind".into(), attribute(TypeId::STR, kind.as_bytes())),
            ("blob".into(), cell(CellKind::Field, TypeId::BYTES, &[n; 4])),
        ];
        // Keys out of creation order, to show results come back by key.
        store
            .create(branch, RecordKey([7 - n; 32]), cells, None)
            .unwrap();
    }
    store.commit(branch).unwrap();

    let even = predicate("kind", CompareOp::Eq, TypeId::STR, b"even", false);
    let big = predicate("n", CompareOp::Gte, TypeId::U64, &4u64.to_be_bytes(), false);
    let one = predicate("n", CompareOp::Eq, TypeId::U64, &1u64.to_be_bytes(), false);
    let keys = |filter: Filter, offset, limit| -> Vec<u8> {
        store
            .query(
                None,
                &Query {
                    filter,
                    page: Page { offset, limit },
                    ..Query::default()
                },
                None,
            )
            .unwrap()
            .into_value()
            .records
            .iter()
            .map(|record| record.key.0[0])
            .collect()
    };

    // n = 2, 4, 6 live at keys 5, 3, 1.
    assert_eq!(
        keys(Filter(vec![AndGroup(vec![even.clone()])]), 0, 10),
        [1, 3, 5]
    );
    assert_eq!(keys(Filter(vec![AndGroup(vec![even.clone()])]), 1, 1), [3]);
    assert_eq!(
        keys(
            Filter(vec![AndGroup(vec![even.clone(), big.clone()])]),
            0,
            10
        ),
        [1, 3]
    );
    assert_eq!(
        keys(
            Filter(vec![
                AndGroup(vec![even.clone(), big.clone()]),
                AndGroup(vec![one.clone()])
            ]),
            0,
            10
        ),
        [1, 3, 6]
    );
    // A negated predicate is applied to the positive matches.
    let not_even = Predicate {
        negated: true,
        ..even.clone()
    };
    assert_eq!(
        keys(
            Filter(vec![AndGroup(vec![big.clone(), not_even.clone()])]),
            0,
            10
        ),
        [2]
    );
    assert_eq!(
        keys(
            Filter(vec![AndGroup(vec![predicate(
                "kind",
                CompareOp::Prefix,
                TypeId::STR,
                b"od",
                false
            )])]),
            0,
            10
        ),
        [2, 4, 6]
    );

    let count = |filter: Filter| store.count(None, &filter, None).unwrap().into_value();
    assert_eq!(count(Filter(vec![AndGroup(vec![even.clone()])])), 3);
    assert_eq!(
        count(Filter(vec![AndGroup(vec![big.clone(), not_even])])),
        1
    );
    assert_eq!(
        store
            .count(None, &Filter::default(), None)
            .map(|c| c.into_value()),
        Err(StoreError::NotImplemented)
    );

    // Sorted descending by n, projected to it.
    let result = store
        .query(
            None,
            &Query {
                filter: Filter(vec![AndGroup(vec![even])]),
                sort: Some(Sort {
                    cell: "n".into(),
                    direction: SortDirection::Descending,
                }),
                page: Page {
                    offset: 0,
                    limit: 10,
                },
                projection: Some(vec!["n".into()]),
                total_matched: true,
            },
            None,
        )
        .unwrap()
        .into_value();
    assert_eq!(result.total_matched, Some(3));
    let ns: Vec<u8> = result
        .records
        .iter()
        .map(|record| record.cell("n").unwrap().value[7])
        .collect();
    assert_eq!(ns, [6, 4, 2]);
    assert!(result.records.iter().all(|record| record.cells.len() == 1));
}
