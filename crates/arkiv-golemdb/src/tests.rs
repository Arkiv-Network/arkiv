use super::*;
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use arkiv_interfaces::store::{CellKind, CompareOp, CostUnits, Page, Predicate, Test, TypeId};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const SEGMENT: &str = "rows";

fn config() -> GolemDbConfig {
    let limits = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 128,
        max_bytes_len: 128 * 1024,
    };
    let record_limits = RecordLimits {
        max_cells: MAX_RECORD_CELLS as u64,
        max_indexed_cells: MAX_RECORD_INDEXED_CELLS as u64,
    };
    GolemDbConfig {
        genesis: Genesis::new(HashAlgorithm::Keccak256, limits, record_limits, None),
        segments: vec![SegmentSpec {
            name: SEGMENT.into(),
            columns: 2,
        }],
        pruning: None,
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

/// GolemDB's own records sit at fixed keys a caller can name; none is Arkiv's.
#[test]
fn golemdb_system_keys_hold_no_record() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    commit_record(&store, 1);
    let branch = store.begin(Some(store.head())).unwrap();
    for system in golemdb_cells::system::ALL {
        for target in [ReadTarget::Commit(store.head()), ReadTarget::Branch(branch)] {
            let read = store.get(target, RecordKey(system.key), None, None);
            assert_eq!(read.map(Metered::into_value), Ok(None), "{system:?}");
        }
    }
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

/// GolemDB takes a `BOOL` of 0 or 1 only, which is why Arkiv stores
/// `$creationFlags`, a byte holding up to 3, as `BYTES`.
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

/// Which is why Arkiv stores them as one `BYTES` byte: an entity with every
/// creation flag set, plus an unindexed blob, writes and reads back whole.
#[test]
fn an_entity_with_creation_flags_round_trips() {
    use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags, Entity};
    use arkiv_interfaces::entity_records::{from_record, record_key, to_cells};

    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    let entity = Entity {
        key: [1; 32],
        creation_flags: CreationFlags::READONLY | CreationFlags::PERMISSIONLESS_EXTENSION,
        attributes: vec![
            Attribute::unindexed(b"body".to_vec(), AttributeValue::Bytes(vec![7; 3])),
            Attribute::new(b"level".to_vec(), AttributeValue::Int(1)),
        ],
        ..Entity::default()
    };
    store
        .create(
            branch,
            record_key(entity.key).unwrap(),
            to_cells(&entity).unwrap(),
            None,
        )
        .unwrap();
    let record = read(&store, ReadTarget::Branch(branch), 1).unwrap();
    let mut back = from_record(&record).unwrap();
    back.attributes.sort_by(|a, b| a.key.cmp(&b.key));
    assert_eq!(back, entity);
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
    limits.genesis.cell_limits.max_str_len += 1;
    let mut record_limits = config();
    record_limits.genesis.record_limits.max_cells += 1;
    let mut segments = config();
    segments.segments[0].columns += 1;
    let mut hash = config();
    hash.genesis.hash_function = HashAlgorithm::Blake3;
    for other in [limits, record_limits, segments, hash] {
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

/// The same writes give the same roots in memory as on disk.
#[test]
fn a_database_in_memory_has_the_roots_of_one_on_disk() {
    let dir = TempDir::new().unwrap();
    let on_disk = open(&dir);
    let in_memory = GolemDbStore::open_memory(&config()).unwrap();
    let roots: Vec<_> = [&on_disk, &in_memory]
        .into_iter()
        .map(|store| {
            let branch = stage(store, 1);
            let commit = store.commit_tagged(branch, [9; 32]).unwrap();
            store.commit_hash(commit).unwrap()
        })
        .collect();
    assert_eq!(roots[0], roots[1]);
}

/// Segments are the host's and the adapter's, so a genesis that declares its
/// own is refused, and nothing is created.
#[test]
fn a_genesis_with_segments_is_refused() {
    let dir = TempDir::new().unwrap();
    let mut config = config();
    config.genesis.immutable_data = Some(golemdb::ImmutableDataConfig {
        shard_span: SHARD_SPAN,
        segments: vec![tags::segment()],
    });
    let error = GolemDbStore::open(dir.path(), &config).unwrap_err();
    assert!(matches!(error, OpenError::InvalidConfig(_)), "{error}");
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
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
            &golemdb::OpenConfig::new(config().full_genesis().unwrap()),
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

/// Arkiv's `Set` writes the cell whether or not the record holds it yet, and
/// with the kind it names. The state layer relies on both: an account record
/// is created with one of its cells and patched with the others, an entity
/// update can add an attribute, and one can make an attribute indexed or not.
#[test]
fn a_patch_adds_missing_cells_and_gives_them_their_kind() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let branch = store.begin(None).unwrap();
    let seven = 7u64.to_be_bytes();
    store
        .create(
            branch,
            RecordKey([1; 32]),
            vec![
                ("n".into(), attribute(TypeId::U64, &seven)),
                ("tag".into(), cell(CellKind::Field, TypeId::STR, b"x")),
            ],
            None,
        )
        .unwrap();
    // Indexed, then not; not, then indexed; and a cell the record lacks.
    let changes = vec![
        (
            "n".into(),
            CellChange::Set(cell(CellKind::Field, TypeId::U64, &seven)),
        ),
        ("tag".into(), CellChange::Set(attribute(TypeId::STR, b"y"))),
        (
            "added".into(),
            CellChange::Set(cell(CellKind::Field, TypeId::STR, b"z")),
        ),
    ];
    store
        .patch(branch, RecordKey([1; 32]), None, changes, None)
        .unwrap();

    let record = read(&store, ReadTarget::Branch(branch), 1).unwrap();
    assert_eq!(
        record.cell("n"),
        Some(&cell(CellKind::Field, TypeId::U64, &seven))
    );
    assert_eq!(record.cell("tag"), Some(&attribute(TypeId::STR, b"y")));
    assert_eq!(
        record.cell("added"),
        Some(&cell(CellKind::Field, TypeId::STR, b"z"))
    );

    // The index follows the kinds: `tag` is found now, and `n` no longer is.
    store.commit(branch).unwrap();
    let tag_is_y = Filter::Predicate(Predicate {
        cell: "tag".into(),
        test: Test::Compare {
            op: CompareOp::Eq,
            type_id: TypeId::STR,
            value: b"y".to_vec(),
        },
    });
    let count = |filter: &Filter| store.count(None, filter, None).map(|n| n.into_value());
    assert_eq!(count(&tag_is_y), Ok(1));
    assert_eq!(count(&n_at_least(7)), Ok(0));
}

/// The filter `n >= at_least`, on [`commit_record`]'s cell.
fn n_at_least(at_least: u64) -> Filter {
    Filter::Predicate(Predicate {
        cell: "n".into(),
        test: Test::Compare {
            op: CompareOp::Gte,
            type_id: TypeId::U64,
            value: at_least.to_be_bytes().to_vec(),
        },
    })
}

/// A count does not page, so GolemDB runs it, at the head only.
#[test]
fn a_count_runs_at_the_head() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let first = commit_record(&store, 1);
    let head = commit_record(&store, 2);
    let count = |at| {
        store
            .count(at, &n_at_least(2), None)
            .map(|n| n.into_value())
    };
    assert_eq!(count(None), Ok(1));
    assert_eq!(count(Some(head)), Ok(1));
    assert_eq!(count(Some(first)), Err(StoreError::HandleInvalid));
}

/// GolemDB pages now, so an Arkiv query runs: the page skips its offset and
/// holds the matches its limit admits. An offset past the last match is an
/// empty page, not an error.
#[test]
fn a_query_pages_at_the_head() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    commit_record(&store, 1);
    commit_record(&store, 2);
    let keys = |offset, limit| {
        let query = Query {
            filter: n_at_least(1),
            page: Page { offset, limit },
            ..Query::default()
        };
        store.query(None, &query, None).map(|page| {
            page.into_value()
                .records
                .iter()
                .map(|record| record.key.0[0])
                .collect::<Vec<u8>>()
        })
    };
    assert_eq!(keys(0, 10), Ok(vec![1, 2]));
    assert_eq!(keys(0, 1), Ok(vec![1]));
    assert_eq!(keys(1, 10), Ok(vec![2]));
    assert_eq!(keys(2, 10), Ok(vec![]));
}

/// GolemDB caps a page at its own query limit, which `arkiv_query`'s largest
/// page matches, so an over-cap page is a structural limit.
#[test]
fn a_page_over_golemdbs_query_limit_is_limit_exceeded() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    commit_record(&store, 1);
    let query = Query {
        filter: n_at_least(1),
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

/// A filter over GolemDB's caps is a structural limit, not a fatal store
/// failure.
#[test]
fn a_query_over_a_cap_is_limit_exceeded() {
    let error = golemdb::ApiError::LimitExceeded {
        limit: "filter depth",
        max: 64,
    };
    assert_eq!(convert::store_error(error), StoreError::LimitExceeded);
}

/// Running out of budget is a verdict on the call, not a store failure, and
/// what was spent by then is on GolemDB's receipt, not in its error.
#[test]
fn running_out_of_budget_reports_what_the_receipt_spent() {
    let mut receipt = golemdb::Receipt::default();
    receipt.cost = 7;
    let outcome = golemdb::Metered::<()>::new(
        Err(golemdb::ApiError::OutOfBudget { required: Some(9) }),
        receipt,
    );
    assert_eq!(
        convert::metered_result(outcome),
        Err(StoreError::OutOfBudget {
            spent: CostUnits(7)
        })
    );
}
