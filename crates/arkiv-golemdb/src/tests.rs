use super::*;
use arkiv_interfaces::store::{CellKind, TypeId};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const SEGMENT: &str = "rows";

fn config() -> GolemDbConfig {
    GolemDbConfig {
        hash_function: HashAlgorithm::Keccak256,
        cell_limits: CellLimits {
            max_cell_name_len: 32,
            max_str_len: 128,
            max_bytes_len: 128 * 1024,
        },
        segments: vec![SegmentSpec {
            name: SEGMENT.into(),
            columns: 2,
        }],
    }
}

/// A row of [`SEGMENT`], told apart by `n`.
fn row(n: u8) -> SegmentRow {
    vec![vec![n], Vec::new()]
}

/// Record `n` and a row on a sealed branch, left uncommitted.
fn stage(store: &GolemDbStore, n: u8) -> BranchId {
    let branch = store.begin(None).unwrap();
    let cells = vec![(
        "n".into(),
        attribute(TypeId::U64, &u64::from(n).to_be_bytes()),
    )];
    store
        .create(branch, RecordKey([n; 32]), cells, None)
        .unwrap();
    store.seal(branch).unwrap();
    store.segment_append(branch, SEGMENT, None, row(n)).unwrap();
    branch
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
fn other_key_cells_read_back_as_bytes32() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    let cells = vec![("ref".into(), attribute(TypeId::KEY, &[5; 32]))];
    store
        .create(branch, RecordKey([1; 32]), cells, None)
        .unwrap();
    let record = read(&store, ReadTarget::Branch(branch), 1).unwrap();
    assert_eq!(record.cell("ref").unwrap().type_id, TypeId::BYTES32);
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
fn no_branch_or_read_over_an_older_commit() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let first = commit_record(&store, 1);
    commit_record(&store, 2);

    assert_eq!(store.begin(Some(first)), Err(StoreError::HandleInvalid));
    assert_eq!(
        read(&store, ReadTarget::Commit(first), 1).map(|_| ()),
        Err(StoreError::HandleInvalid)
    );
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

    let mut limits = config();
    limits.cell_limits.max_str_len += 1;
    let mut segments = config();
    segments.segments[0].columns += 1;
    let mut hash = config();
    hash.hash_function = HashAlgorithm::Blake3;
    for other in [limits, segments, hash] {
        assert!(matches!(
            GolemDbStore::open(dir.path(), &other),
            Err(OpenError::GenesisMismatch)
        ));
    }
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

/// A child process commits record 1 and a row under a tag, stages record 2
/// and another row on a branch it never commits, and is killed while it holds
/// the store, so no cleanup runs.
#[test]
fn a_killed_process_keeps_only_its_commits() {
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        let store = GolemDbStore::open(dir.as_ref(), &config()).unwrap();
        store.commit_tagged(stage(&store, 1), [1; 32]).unwrap();
        stage(&store, 2);
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
    assert_eq!(
        store.segment_get(SEGMENT, SegmentAddress::Ordinal(0)),
        Ok(Some(row(1)))
    );
    assert_eq!(
        store.segment_get(SEGMENT, SegmentAddress::Ordinal(1)),
        Ok(None)
    );
    assert_eq!(store.commit_by_tag([1; 32]), Ok(Some(CommitId(1))));
    assert_eq!(store.tag_of(CommitId(1)), Ok(Some([1; 32])));
}

/// Rows staged on a sealed branch appear with its commit, each readable by
/// its ordinal, by its key if it has one, and among its commit's rows.
#[test]
fn segment_rows_appear_with_their_commit() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    store.seal(branch).unwrap();
    let key = SegmentKey([7; 32]);
    let writes = [(SEGMENT, Some(key), row(0)), (SEGMENT, None, row(1))];
    assert_eq!(store.segment_append_many(branch, &writes), Ok(vec![0, 1]));
    assert_eq!(
        store.segment_get(SEGMENT, SegmentAddress::Ordinal(0)),
        Ok(None)
    );

    let commit = store.commit(branch).unwrap();
    assert_eq!(
        store.segment_rows_of(SEGMENT, commit),
        Ok(vec![row(0), row(1)])
    );
    assert_eq!(store.segment_rows_of(SEGMENT, CommitId(0)), Ok(vec![]));
    assert_eq!(
        store.segment_rows_of(SEGMENT, CommitId(commit.0 + 1)),
        Err(StoreError::NotFound)
    );
    assert_eq!(
        store.segment_get(SEGMENT, SegmentAddress::Key(key)),
        Ok(Some(row(0)))
    );
    assert_eq!(
        store.segment_get(SEGMENT, SegmentAddress::Ordinal(1)),
        Ok(Some(row(1)))
    );
    assert_eq!(
        store.segment_get(SEGMENT, SegmentAddress::Ordinal(2)),
        Ok(None)
    );
}

/// Arkiv appends only after seal, so a row can never move a root.
#[test]
fn appends_need_a_sealed_branch() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    assert_eq!(
        store.segment_append(branch, SEGMENT, None, row(0)),
        Err(StoreError::NotSealed)
    );
    assert_eq!(
        store.segment_append_many(branch, &[(SEGMENT, None, row(0))]),
        Err(StoreError::NotSealed)
    );
}

/// A key names one row: a second row under it is refused, whether the first
/// is staged or committed.
#[test]
fn a_segment_key_names_one_row() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let key = Some(SegmentKey([7; 32]));
    let branch = store.begin(None).unwrap();
    store.seal(branch).unwrap();
    store.segment_append(branch, SEGMENT, key, row(0)).unwrap();
    assert_eq!(
        store.segment_append(branch, SEGMENT, key, row(1)),
        Err(StoreError::AlreadyExists)
    );
    store.commit(branch).unwrap();

    let branch = store.begin(None).unwrap();
    store.seal(branch).unwrap();
    assert_eq!(
        store.segment_append(branch, SEGMENT, key, row(1)),
        Err(StoreError::AlreadyExists)
    );
}

/// A row goes only to a segment the host declared, with that segment's column
/// count.
#[test]
fn a_row_must_fit_a_declared_segment() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    store.seal(branch).unwrap();
    assert_eq!(
        store.segment_append(branch, "other", None, row(0)),
        Err(StoreError::NotFound)
    );
    assert_eq!(
        store.segment_append(branch, SEGMENT, None, vec![vec![0]]),
        Err(StoreError::InvalidArgument)
    );
    assert_eq!(
        store.segment_append(branch, tags::SEGMENT, None, row(0)),
        Err(StoreError::InvalidArgument)
    );
}

/// Every commit keeps its tag, if it has one, and the state root it sealed.
#[test]
fn a_commit_keeps_its_tag_and_its_root() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let commit = |n: u8, tag: Option<[u8; 32]>| {
        let branch = stage(&store, n);
        let sealed = store.seal(branch).unwrap();
        let committed = match tag {
            Some(tag) => store.commit_tagged(branch, tag),
            None => store.commit(branch),
        };
        assert_eq!(committed, Ok(sealed.commit_nr));
        sealed
    };
    let untagged = commit(1, None);
    let tagged = commit(2, Some([9; 32]));

    assert_eq!(store.commit_by_tag([9; 32]), Ok(Some(tagged.commit_nr)));
    assert_eq!(store.tag_of(tagged.commit_nr), Ok(Some([9; 32])));
    assert_eq!(store.tag_of(untagged.commit_nr), Ok(None));
    for sealed in [untagged, tagged] {
        assert_eq!(store.commit_hash(sealed.commit_nr), Ok(sealed.state_root));
    }

    // Nothing for an unused tag, the empty genesis, or a commit yet to come.
    assert_eq!(store.commit_by_tag([8; 32]), Ok(None));
    assert_eq!(store.tag_of(CommitId(0)), Ok(None));
    assert_eq!(store.tag_of(CommitId(3)), Ok(None));
    assert_eq!(store.commit_hash(CommitId(0)), Err(StoreError::NotFound));
}

/// A tag names one commit. A refused tag leaves the branch usable, and it
/// still commits with a single row.
#[test]
fn a_tag_names_one_commit() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let first = store.commit_tagged(stage(&store, 1), [9; 32]).unwrap();
    let branch = stage(&store, 2);
    assert_eq!(
        store.commit_tagged(branch, [9; 32]),
        Err(StoreError::AlreadyExists)
    );

    let second = store.commit_tagged(branch, [8; 32]).unwrap();
    assert_eq!(second, CommitId(first.0 + 1));
    assert_eq!(store.tag_of(second), Ok(Some([8; 32])));
    assert_eq!(store.commit_by_tag([9; 32]), Ok(Some(first)));
}

/// Tags and roots are read back from disk after a reopen, both ways.
#[test]
fn tags_and_roots_survive_a_reopen() {
    let dir = TempDir::new().unwrap();
    let sealed = {
        let store = open(&dir);
        let branch = stage(&store, 1);
        let sealed = store.seal(branch).unwrap();
        store.commit_tagged(branch, [9; 32]).unwrap();
        sealed
    };

    let store = open(&dir);
    assert_eq!(store.commit_by_tag([9; 32]), Ok(Some(sealed.commit_nr)));
    assert_eq!(store.tag_of(sealed.commit_nr), Ok(Some([9; 32])));
    assert_eq!(store.commit_hash(sealed.commit_nr), Ok(sealed.state_root));
}

/// `commit_tags` is the adapter's own, so a host segment of that name is a
/// duplicate GolemDB refuses at open.
#[test]
fn a_host_segment_may_not_be_named_commit_tags() {
    let dir = TempDir::new().unwrap();
    let mut config = config();
    config.segments.push(SegmentSpec {
        name: tags::SEGMENT.into(),
        columns: 1,
    });
    let error = GolemDbStore::open(dir.path(), &config).unwrap_err();
    assert!(error.to_string().contains("duplicate"), "{error}");
}

/// A commit's tag row is found by the commit that published it, not by
/// position, so a stray second row spoils only that commit's tag.
#[test]
fn a_stray_tag_row_spoils_only_its_commit() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = stage(&store, 1);
    // What a second, racing commit of the same branch would stage.
    let stray = TagRow {
        commit: 1,
        tag: None,
        state_root: [0; 32],
    };
    store
        .database
        .immutable_data_append(branch_id(branch), tags::SEGMENT, None, stray.to_row())
        .unwrap();
    let spoiled = store.commit(branch).unwrap();
    let next = store.commit_tagged(stage(&store, 2), [9; 32]).unwrap();

    assert_eq!(store.tag_of(spoiled), Err(StoreError::Internal));
    assert_eq!(store.tag_of(next), Ok(Some([9; 32])));
    assert!(store.commit_hash(next).is_ok());
}

/// A commit that fails in storage takes its tag row back off: a retry fails
/// the same way, not on its own leftover row, and the tag stays free.
#[test]
fn a_failed_commit_leaves_no_tag_row() {
    let dir = TempDir::new().unwrap();
    // Too small for the branch below, so its commit fails as full.
    let options = golemdb::MdbxOptions {
        max_map_size: 1024 * 1024,
        growth_step: 64 * 1024,
        ..Default::default()
    };
    let store = GolemDbStore {
        database: golemdb::Database::open_with_options(
            dir.path(),
            &golemdb::OpenConfig::new(config().genesis()),
            options,
        )
        .unwrap(),
    };
    let branch = store.begin(None).unwrap();
    for n in 0..10 {
        let payload = vec![n; 128 * 1024];
        let cells = vec![("p".into(), cell(CellKind::Field, TypeId::BYTES, &payload))];
        store
            .create(branch, RecordKey([n; 32]), cells, None)
            .unwrap();
    }
    assert_eq!(
        store.commit_tagged(branch, [9; 32]),
        Err(StoreError::Internal)
    );
    assert_eq!(
        store.commit_tagged(branch, [9; 32]),
        Err(StoreError::Internal)
    );

    store.discard(branch).unwrap();
    let commit = store.commit_tagged(stage(&store, 1), [9; 32]).unwrap();
    assert_eq!(commit, CommitId(1));
    assert_eq!(store.tag_of(commit), Ok(Some([9; 32])));
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
