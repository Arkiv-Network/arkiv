use super::*;
use arkiv_interfaces::store::{CellKind, TypeId};
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

    let mut other = config();
    other.cell_limits.max_str_len += 1;
    assert!(GolemDbStore::open(dir.path(), &other).is_err());
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
