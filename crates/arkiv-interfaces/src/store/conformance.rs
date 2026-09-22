//! The store contract, as executable assertions.
//!
//! Every [`Store`] implementation — the in-memory reference, a mock over some
//! other engine, the real store — must pass [`run_all`] unchanged. That is the
//! whole point of the seam: if two implementations both pass, a host cannot
//! tell them apart, and swapping one for the other cannot move consensus.
//!
//! Written against `alloc` only, so it runs wherever the trait does.
//!
//! Usage, from an implementation's test module:
//!
//! ```ignore
//! #[test]
//! fn conformance() {
//!     arkiv_interfaces::store::conformance::run_all(&MemStore::new);
//! }
//! ```

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use super::*;

/// Every [`Store`] assertion, in one call.
pub fn run_all<S: Store>(new: &dyn Fn() -> S) {
    genesis_head_is_zero(new);
    create_then_read_on_branch(new);
    read_your_own_writes(new);
    absent_record_reads_none(new);
    create_collision_is_already_exists(new);
    commit_advances_head(new);
    committed_state_is_readable_at_commit(new);
    uncommitted_writes_invisible_to_origin_commit(new);
    fork_isolates_child_writes(new);
    discard_drops_child_writes(new);
    merge_folds_child_into_parent(new);
    merge_guard_fires_when_parent_advanced(new);
    commit_guard_fires_when_origin_not_head(new);
    consumed_handle_is_invalid(new);
    child_branch_cannot_commit(new);
    concurrent_root_branches_are_independent(new);
    patch_bumps_record_version(new);
    patch_version_guard(new);
    zero_version_guard_never_matches(new);
    patch_may_not_remove_last_cell(new);
    delete_removes_record(new);
    reserved_cell_name_rejected(new);
    field_only_type_cannot_be_attribute(new);
    invalid_type_id_rejected(new);
    branch_hash_ignores_record_version(new);
    branch_hash_tracks_content(new);
    query_sees_committed_state(new);
    query_dnf_unions_and_dedups(new);
    query_defaults_to_key_order(new);
    query_range_on_eq_only_type_is_invalid(new);
}

/// Every [`StoreExt`] assertion. Separate because the extensions are additions
/// to `golem-db-api.md`, not part of it — an implementation may legitimately
/// pass [`run_all`] and not yet implement these.
pub fn run_all_ext<S: StoreExt>(new: &dyn Fn() -> S) {
    commit_tag_round_trips(new);
    commit_hash_matches_branch_hash(new);
    changes_report_create_patch_delete(new);
    apply_batch_matches_individual_writes(new);
    get_many_matches_individual_gets(new);
    retention_covers_head(new);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn key(n: u8) -> RecordKey {
    let mut k = [0u8; 32];
    k[31] = n;
    RecordKey(k)
}

fn u64_cell(v: u64) -> Cell {
    Cell::attribute(TypeId::U64, v.to_be_bytes().to_vec())
}

fn str_cell(v: &str) -> Cell {
    Cell::attribute(TypeId::STR, v.as_bytes().to_vec())
}

fn cells(pairs: &[(&str, Cell)]) -> Vec<(CellName, Cell)> {
    pairs
        .iter()
        .map(|(n, c)| (n.to_string(), c.clone()))
        .collect()
}

/// `create` with no budget, unwrapping the receipt.
fn put<S: Store>(s: &mut S, b: BranchId, k: RecordKey, pairs: &[(&str, Cell)]) {
    s.create(b, k, cells(pairs), None)
        .expect("create succeeds")
        .into_value();
}

fn read<S: Store>(s: &S, t: ReadTarget, k: RecordKey) -> Option<Record> {
    s.get(t, k, None, None).expect("get succeeds").into_value()
}

fn eq_filter(cell: &str, type_id: TypeId, value: Vec<u8>) -> Filter {
    Filter(vec![AndGroup(vec![Predicate {
        cell: cell.to_string(),
        op: CompareOp::Eq,
        type_id,
        value,
        negated: false,
    }])])
}

// ---------------------------------------------------------------------------
// commits and branches
// ---------------------------------------------------------------------------

fn genesis_head_is_zero<S: Store>(new: &dyn Fn() -> S) {
    assert_eq!(new().head(), CommitId::GENESIS);
}

fn create_then_read_on_branch<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(7))]);

    let got = read(&s, ReadTarget::Branch(b), key(1)).expect("record present");
    assert_eq!(got.key, key(1));
    assert_eq!(got.version, RecordVersion(1), "a new record starts at 1");
    assert_eq!(got.cell("n"), Some(&u64_cell(7)));
}

fn read_your_own_writes<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    s.patch(
        b,
        key(1),
        None,
        vec![("n".to_string(), CellChange::Set(u64_cell(2)))],
        None,
    )
    .unwrap();

    let got = read(&s, ReadTarget::Branch(b), key(1)).unwrap();
    assert_eq!(got.cell("n"), Some(&u64_cell(2)));
}

fn absent_record_reads_none<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    assert!(read(&s, ReadTarget::Branch(b), key(99)).is_none());
}

fn create_collision_is_already_exists<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    assert_eq!(
        s.create(b, key(1), cells(&[("n", u64_cell(2))]), None)
            .unwrap_err(),
        StoreError::AlreadyExists
    );
}

fn commit_advances_head<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    let c = s.commit(b).unwrap();

    assert_eq!(c, CommitId(1), "commits are gapless from genesis");
    assert_eq!(s.head(), c);
}

fn committed_state_is_readable_at_commit<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(7))]);
    let c = s.commit(b).unwrap();

    let got = read(&s, ReadTarget::Commit(c), key(1)).expect("visible at its commit");
    assert_eq!(got.cell("n"), Some(&u64_cell(7)));
}

fn uncommitted_writes_invisible_to_origin_commit<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);

    assert!(
        read(&s, ReadTarget::Commit(CommitId::GENESIS), key(1)).is_none(),
        "a branch's writes never reach its origin commit"
    );
}

fn fork_isolates_child_writes<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let parent = s.begin(None).unwrap();
    put(&mut s, parent, key(1), &[("n", u64_cell(1))]);

    let child = s.fork(parent).unwrap();
    put(&mut s, child, key(2), &[("n", u64_cell(2))]);

    assert!(
        read(&s, ReadTarget::Branch(child), key(1)).is_some(),
        "child sees the parent's state at fork time"
    );
    assert!(
        read(&s, ReadTarget::Branch(parent), key(2)).is_none(),
        "parent does not see the child's writes before merge"
    );
}

fn discard_drops_child_writes<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let parent = s.begin(None).unwrap();
    let child = s.fork(parent).unwrap();
    put(&mut s, child, key(2), &[("n", u64_cell(2))]);
    s.discard(child).unwrap();

    assert!(read(&s, ReadTarget::Branch(parent), key(2)).is_none());
}

fn merge_folds_child_into_parent<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let parent = s.begin(None).unwrap();
    let before = s.branch_info(parent).unwrap().version;

    let child = s.fork(parent).unwrap();
    put(&mut s, child, key(2), &[("n", u64_cell(2))]);
    let after = s.merge(child).unwrap();

    assert_eq!(
        after.0,
        before.0 + 1,
        "a merged child counts as exactly one batch"
    );
    assert!(read(&s, ReadTarget::Branch(parent), key(2)).is_some());
}

fn merge_guard_fires_when_parent_advanced<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let parent = s.begin(None).unwrap();
    let child = s.fork(parent).unwrap();

    // The parent moves on after the fork, breaking the sequential discipline.
    put(&mut s, parent, key(1), &[("n", u64_cell(1))]);

    assert_eq!(s.merge(child).unwrap_err(), StoreError::Conflict);
}

fn commit_guard_fires_when_origin_not_head<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let first = s.begin(None).unwrap();
    let stale = s.begin(None).unwrap();

    put(&mut s, first, key(1), &[("n", u64_cell(1))]);
    s.commit(first).unwrap();

    put(&mut s, stale, key(2), &[("n", u64_cell(2))]);
    assert_eq!(
        s.commit(stale).unwrap_err(),
        StoreError::Conflict,
        "a second block at one height must not commit"
    );
}

fn consumed_handle_is_invalid<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    s.commit(b).unwrap();

    assert_eq!(s.branch_info(b).unwrap_err(), StoreError::HandleInvalid);
    assert_eq!(
        s.create(b, key(1), cells(&[("n", u64_cell(1))]), None)
            .unwrap_err(),
        StoreError::HandleInvalid
    );
}

fn child_branch_cannot_commit<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let parent = s.begin(None).unwrap();
    let child = s.fork(parent).unwrap();

    assert_eq!(s.commit(child).unwrap_err(), StoreError::HandleInvalid);
}

fn concurrent_root_branches_are_independent<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let a = s.begin(None).unwrap();
    let b = s.begin(None).unwrap();

    put(&mut s, a, key(1), &[("n", u64_cell(1))]);
    put(&mut s, b, key(2), &[("n", u64_cell(2))]);

    assert!(read(&s, ReadTarget::Branch(a), key(2)).is_none());
    assert!(read(&s, ReadTarget::Branch(b), key(1)).is_none());
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

fn patch_bumps_record_version<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);

    let v = s
        .patch(
            b,
            key(1),
            None,
            vec![("n".to_string(), CellChange::Set(u64_cell(2)))],
            None,
        )
        .unwrap()
        .into_value();
    assert_eq!(v, RecordVersion(2));
}

fn patch_version_guard<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);

    let change = || vec![("n".to_string(), CellChange::Set(u64_cell(9)))];
    assert_eq!(
        s.patch(b, key(1), Some(RecordVersion(2)), change(), None)
            .unwrap_err(),
        StoreError::Conflict
    );
    assert!(
        s.patch(b, key(1), Some(RecordVersion(1)), change(), None)
            .is_ok(),
        "the matching guard passes"
    );
}

fn zero_version_guard_never_matches<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);

    assert_eq!(
        s.patch(
            b,
            key(1),
            Some(RecordVersion(0)),
            vec![("n".to_string(), CellChange::Set(u64_cell(2)))],
            None
        )
        .unwrap_err(),
        StoreError::Conflict,
        "0 is a guard that always fails, not a don't-care sentinel"
    );
}

fn patch_may_not_remove_last_cell<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);

    assert_eq!(
        s.patch(
            b,
            key(1),
            None,
            vec![("n".to_string(), CellChange::Remove)],
            None
        )
        .unwrap_err(),
        StoreError::InvalidArgument
    );
}

fn delete_removes_record<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    s.delete(b, key(1), None, None).unwrap();

    assert!(read(&s, ReadTarget::Branch(b), key(1)).is_none());
    assert_eq!(
        s.delete(b, key(1), None, None).unwrap_err(),
        StoreError::NotFound
    );
}

fn reserved_cell_name_rejected<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();

    assert_eq!(
        s.create(b, key(1), cells(&[("#version", u64_cell(1))]), None)
            .unwrap_err(),
        StoreError::InvalidArgument,
        "`#` is reserved for store-internal meta entries"
    );
}

fn field_only_type_cannot_be_attribute<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    let bad = Cell::attribute(TypeId::BYTES, vec![1, 2, 3]);

    assert_eq!(
        s.create(b, key(1), cells(&[("blob", bad)]), None)
            .unwrap_err(),
        StoreError::InvalidArgument
    );
    // The same value as a field is fine.
    let ok = Cell::field(TypeId::BYTES, vec![1, 2, 3]);
    assert!(s.create(b, key(1), cells(&[("blob", ok)]), None).is_ok());
}

fn invalid_type_id_rejected<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    let bad = Cell::field(TypeId(0), vec![1]);

    assert_eq!(
        s.create(b, key(1), cells(&[("x", bad)]), None).unwrap_err(),
        StoreError::InvalidArgument,
        "type id 0 is reserved"
    );
}

// ---------------------------------------------------------------------------
// commitment
// ---------------------------------------------------------------------------

fn branch_hash_ignores_record_version<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    let before = s.branch_hash(b).unwrap();

    // A write that leaves the record logically unchanged still bumps its
    // version — and must not move the digest.
    s.patch(
        b,
        key(1),
        None,
        vec![("n".to_string(), CellChange::Set(u64_cell(1)))],
        None,
    )
    .unwrap();

    assert_eq!(
        s.branch_hash(b).unwrap(),
        before,
        "versions are coordination metadata, excluded from the commitment"
    );
}

fn branch_hash_tracks_content<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    let empty = s.branch_hash(b).unwrap();

    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    let one = s.branch_hash(b).unwrap();
    assert_ne!(one, empty);

    s.patch(
        b,
        key(1),
        None,
        vec![("n".to_string(), CellChange::Set(u64_cell(2)))],
        None,
    )
    .unwrap();
    assert_ne!(s.branch_hash(b).unwrap(), one);

    // And it is a pure function of content, not of history.
    s.delete(b, key(1), None, None).unwrap();
    assert_eq!(
        s.branch_hash(b).unwrap(),
        empty,
        "returning to the same content returns to the same digest"
    );
}

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

fn query_sees_committed_state<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("kind", str_cell("a"))]);
    put(&mut s, b, key(2), &[("kind", str_cell("b"))]);
    let c = s.commit(b).unwrap();

    let q = Query {
        filter: eq_filter("kind", TypeId::STR, b"a".to_vec()),
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };
    let got = s.query(Some(c), &q, None).unwrap().into_value();
    assert_eq!(got.records.len(), 1);
    assert_eq!(got.records[0].key, key(1));

    assert_eq!(
        s.count(Some(c), &q.filter, None).unwrap().into_value(),
        1,
        "count agrees with query"
    );
}

fn query_dnf_unions_and_dedups<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(
        &mut s,
        b,
        key(1),
        &[("kind", str_cell("a")), ("n", u64_cell(1))],
    );
    put(
        &mut s,
        b,
        key(2),
        &[("kind", str_cell("b")), ("n", u64_cell(2))],
    );
    let c = s.commit(b).unwrap();

    // Two groups both matching record 1: it must come back once.
    let filter = Filter(vec![
        AndGroup(vec![Predicate {
            cell: "kind".to_string(),
            op: CompareOp::Eq,
            type_id: TypeId::STR,
            value: b"a".to_vec(),
            negated: false,
        }]),
        AndGroup(vec![Predicate {
            cell: "n".to_string(),
            op: CompareOp::Eq,
            type_id: TypeId::U64,
            value: 1u64.to_be_bytes().to_vec(),
            negated: false,
        }]),
    ]);
    let q = Query {
        filter,
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };
    let got = s.query(Some(c), &q, None).unwrap().into_value();
    assert_eq!(
        got.records.len(),
        1,
        "a record matching both groups is returned once"
    );
}

fn query_defaults_to_key_order<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    for n in [3u8, 1, 2] {
        put(&mut s, b, key(n), &[("kind", str_cell("a"))]);
    }
    let c = s.commit(b).unwrap();

    let q = Query {
        filter: eq_filter("kind", TypeId::STR, b"a".to_vec()),
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };
    let got = s.query(Some(c), &q, None).unwrap().into_value();
    let keys: Vec<RecordKey> = got.records.iter().map(|r| r.key).collect();
    assert_eq!(keys, vec![key(1), key(2), key(3)]);
}

fn query_range_on_eq_only_type_is_invalid<S: Store>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(
        &mut s,
        b,
        key(1),
        &[("h", Cell::attribute(TypeId::BYTES32, vec![0u8; 32]))],
    );
    let c = s.commit(b).unwrap();

    let filter = Filter(vec![AndGroup(vec![Predicate {
        cell: "h".to_string(),
        op: CompareOp::Gt,
        type_id: TypeId::BYTES32,
        value: vec![0u8; 32],
        negated: false,
    }])]);
    assert_eq!(
        s.count(Some(c), &filter, None).unwrap_err(),
        StoreError::InvalidQuery,
        "bytes32 indexes equality only"
    );
}

// ---------------------------------------------------------------------------
// extensions
// ---------------------------------------------------------------------------

fn commit_tag_round_trips<S: StoreExt>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    let tag = [0xAB; 32];
    let c = s.commit_tagged(b, tag).unwrap();

    assert_eq!(s.commit_by_tag(tag).unwrap(), Some(c));
    assert_eq!(s.commit_by_tag([0x00; 32]).unwrap(), None);
}

fn commit_hash_matches_branch_hash<S: StoreExt>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    let sealed = s.branch_hash(b).unwrap();
    let c = s.commit(b).unwrap();

    assert_eq!(
        s.commit_hash(c).unwrap(),
        sealed,
        "a commit's digest is the digest its branch had at seal time"
    );
}

fn changes_report_create_patch_delete<S: StoreExt>(new: &dyn Fn() -> S) {
    let mut s = new();

    let b1 = s.begin(None).unwrap();
    put(&mut s, b1, key(1), &[("n", u64_cell(1))]);
    let c1 = s.commit(b1).unwrap();

    let created = s.changes(c1).unwrap();
    assert_eq!(created.len(), 1);
    assert!(created[0].before.is_none() && created[0].after.is_some());

    let b2 = s.begin(None).unwrap();
    s.patch(
        b2,
        key(1),
        None,
        vec![("n".to_string(), CellChange::Set(u64_cell(2)))],
        None,
    )
    .unwrap();
    s.delete(b2, key(2), None, None).ok();
    let c2 = s.commit(b2).unwrap();

    let patched = s.changes(c2).unwrap();
    assert_eq!(patched.len(), 1);
    assert!(patched[0].before.is_some() && patched[0].after.is_some());

    let b3 = s.begin(None).unwrap();
    s.delete(b3, key(1), None, None).unwrap();
    let c3 = s.commit(b3).unwrap();

    let deleted = s.changes(c3).unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].before.is_some() && deleted[0].after.is_none());
}

fn apply_batch_matches_individual_writes<S: StoreExt>(new: &dyn Fn() -> S) {
    // One store gets a batch, the other the same writes one at a time. The
    // resulting digests must be identical — that is what makes batching a
    // transport optimization rather than a semantic one.
    let mut batched = new();
    let bb = batched.begin(None).unwrap();
    batched
        .apply(
            bb,
            vec![
                WriteOp::Create {
                    key: key(1),
                    cells: cells(&[("n", u64_cell(1))]),
                },
                WriteOp::Create {
                    key: key(2),
                    cells: cells(&[("n", u64_cell(2))]),
                },
                WriteOp::Patch {
                    key: key(1),
                    expected_version: None,
                    changes: vec![("n".to_string(), CellChange::Set(u64_cell(9)))],
                },
            ],
            None,
        )
        .unwrap();

    let mut serial = new();
    let sb = serial.begin(None).unwrap();
    put(&mut serial, sb, key(1), &[("n", u64_cell(1))]);
    put(&mut serial, sb, key(2), &[("n", u64_cell(2))]);
    serial
        .patch(
            sb,
            key(1),
            None,
            vec![("n".to_string(), CellChange::Set(u64_cell(9)))],
            None,
        )
        .unwrap();

    assert_eq!(
        batched.branch_hash(bb).unwrap(),
        serial.branch_hash(sb).unwrap()
    );
}

fn get_many_matches_individual_gets<S: StoreExt>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);

    let keys = [key(1), key(2)];
    let many = s
        .get_many(ReadTarget::Branch(b), &keys, None, None)
        .unwrap()
        .into_value();
    let one: Vec<Option<Record>> = keys
        .iter()
        .map(|k| read(&s, ReadTarget::Branch(b), *k))
        .collect();

    assert_eq!(many, one);
}

fn retention_covers_head<S: StoreExt>(new: &dyn Fn() -> S) {
    let mut s = new();
    let b = s.begin(None).unwrap();
    put(&mut s, b, key(1), &[("n", u64_cell(1))]);
    let c = s.commit(b).unwrap();

    let (oldest, newest) = s.retention();
    assert_eq!(newest, c);
    assert!(oldest <= newest);
}
