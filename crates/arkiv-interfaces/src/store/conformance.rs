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
//!     arkiv_interfaces::store::conformance::run_all(&MyStore::new);
//! }
//! ```
//!
//! [`MemStore`](super::reference::MemStore) is the oracle this suite is proved
//! against, so a green run there means a failure elsewhere is the
//! implementation's and not the assertion's.
//!
//! Each assertion takes a **constructor** rather than a store, so every one
//! starts from genesis and none can be polluted by an earlier failure.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use super::*;

/// Run every [`Store`] assertion against stores built by `new_store`.
///
/// Panics on the first violation, naming the rule that broke.
pub fn run_all<S: Store>(new_store: &dyn Fn() -> S) {
    genesis_head_is_zero(new_store);
    create_then_read_on_branch(new_store);
    read_your_own_writes(new_store);
    absent_record_reads_none(new_store);
    create_collision_is_already_exists(new_store);
    commit_advances_head(new_store);
    committed_state_is_readable_at_commit(new_store);
    uncommitted_writes_invisible_to_origin_commit(new_store);
    fork_isolates_child_writes(new_store);
    discard_drops_child_writes(new_store);
    merge_folds_child_into_parent(new_store);
    merge_guard_fires_when_parent_advanced(new_store);
    commit_guard_fires_when_origin_not_head(new_store);
    consumed_handle_is_invalid(new_store);
    child_branch_cannot_commit(new_store);
    concurrent_root_branches_are_independent(new_store);
    patch_bumps_record_version(new_store);
    patch_version_guard_rejects_stale_reader(new_store);
    zero_version_guard_never_matches(new_store);
    patch_may_not_remove_last_cell(new_store);
    delete_removes_record(new_store);
    reserved_cell_name_rejected(new_store);
    field_only_type_cannot_be_attribute(new_store);
    invalid_type_id_rejected(new_store);
    branch_hash_ignores_record_version(new_store);
    branch_hash_tracks_content(new_store);
    query_sees_committed_state(new_store);
    query_dnf_unions_and_dedups(new_store);
    query_defaults_to_key_order(new_store);
    query_range_on_eq_only_type_is_invalid(new_store);
    genesis_lands_as_the_first_commit(new_store);
    genesis_refuses_to_apply_twice(new_store);
    genesis_is_readable_at_its_commit(new_store);
}

/// Run every [`StoreExt`] assertion against stores built by `new_store`.
///
/// Separate from [`run_all`] because the extensions are additions to
/// `golem-db-api.md`, not part of it — an implementation may legitimately
/// conform to the spec and not yet implement these.
pub fn run_all_ext<S: StoreExt>(new_store: &dyn Fn() -> S) {
    commit_tag_round_trips(new_store);
    commit_hash_matches_branch_hash(new_store);
    changes_report_create_patch_delete(new_store);
    apply_batch_matches_individual_writes(new_store);
    get_many_matches_individual_gets(new_store);
    retention_covers_head(new_store);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A distinct record key per `ordinal`, ordered so that `record_key(1)` sorts
/// before `record_key(2)` — which the default result-order assertion relies on.
fn record_key(ordinal: u8) -> RecordKey {
    let mut bytes = [0u8; 32];
    bytes[31] = ordinal;
    RecordKey(bytes)
}

/// An indexed `u64` cell, canonically big-endian.
fn u64_attribute(value: u64) -> Cell {
    Cell::attribute(TypeId::U64, value.to_be_bytes().to_vec())
}

/// An indexed `str` cell, canonically raw UTF-8.
fn str_attribute(value: &str) -> Cell {
    Cell::attribute(TypeId::STR, value.as_bytes().to_vec())
}

/// Turn borrowed `(name, cell)` pairs into the owned map the trait takes.
fn cell_map(pairs: &[(&str, Cell)]) -> Vec<(CellName, Cell)> {
    pairs
        .iter()
        .map(|(name, cell)| (name.to_string(), cell.clone()))
        .collect()
}

/// [`Store::create`] with no budget, asserting success and dropping the
/// receipt — for the many assertions that are about something else.
fn create_record<S: Store>(
    store: &mut S,
    branch: BranchId,
    key: RecordKey,
    pairs: &[(&str, Cell)],
) {
    store
        .create(branch, key, cell_map(pairs), None)
        .expect("create succeeds")
        .into_value();
}

/// [`Store::get`] with no projection and no budget, asserting the call itself
/// succeeded and returning whether the record was there.
fn read_record<S: Store>(store: &S, target: ReadTarget, key: RecordKey) -> Option<Record> {
    store
        .get(target, key, None, None)
        .expect("get succeeds")
        .into_value()
}

/// A one-group, one-predicate DNF: `cell == value`.
fn equals_filter(cell: &str, type_id: TypeId, value: Vec<u8>) -> Filter {
    Filter(vec![AndGroup(vec![Predicate {
        cell: cell.to_string(),
        op: CompareOp::Eq,
        type_id,
        value,
        negated: false,
    }])])
}

/// The single-cell change list most patch assertions use.
fn set_cell(name: &str, cell: Cell) -> Vec<(CellName, CellChange)> {
    vec![(name.to_string(), CellChange::Set(cell))]
}

// ---------------------------------------------------------------------------
// commits and branches
// ---------------------------------------------------------------------------

/// A fresh store is at genesis: commit 0, empty state.
fn genesis_head_is_zero<S: Store>(new_store: &dyn Fn() -> S) {
    assert_eq!(new_store().head(), CommitId::GENESIS);
}

/// A created record reads back with its key, its cells, and version 1.
fn create_then_read_on_branch<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(7))],
    );

    let found = read_record(&store, ReadTarget::Branch(branch), record_key(1))
        .expect("the record just created is present");
    assert_eq!(found.key, record_key(1));
    assert_eq!(found.version, RecordVersion(1), "a new record starts at 1");
    assert_eq!(found.cell("n"), Some(&u64_attribute(7)));
}

/// A branch read sees that branch's own uncommitted writes.
fn read_your_own_writes<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    store
        .patch(
            branch,
            record_key(1),
            None,
            set_cell("n", u64_attribute(2)),
            None,
        )
        .unwrap();

    let found = read_record(&store, ReadTarget::Branch(branch), record_key(1)).unwrap();
    assert_eq!(found.cell("n"), Some(&u64_attribute(2)));
}

/// Reading a key that was never written is `None`, not an error.
fn absent_record_reads_none<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    assert!(read_record(&store, ReadTarget::Branch(branch), record_key(99)).is_none());
}

/// Creating over an existing key is refused; `create` never overwrites.
fn create_collision_is_already_exists<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert_eq!(
        store
            .create(
                branch,
                record_key(1),
                cell_map(&[("n", u64_attribute(2))]),
                None
            )
            .unwrap_err(),
        StoreError::AlreadyExists
    );
}

/// Committing assigns `head + 1`, gapless from genesis.
fn commit_advances_head<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let committed = store.commit(branch).unwrap();

    assert_eq!(committed, CommitId(1), "commits are gapless from genesis");
    assert_eq!(store.head(), committed);
}

/// What a branch committed is readable at the commit it produced.
fn committed_state_is_readable_at_commit<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(7))],
    );
    let committed = store.commit(branch).unwrap();

    let found = read_record(&store, ReadTarget::Commit(committed), record_key(1))
        .expect("visible at its own commit");
    assert_eq!(found.cell("n"), Some(&u64_attribute(7)));
}

/// A branch's writes never leak backwards into the commit it forked from.
fn uncommitted_writes_invisible_to_origin_commit<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert!(
        read_record(&store, ReadTarget::Commit(CommitId::GENESIS), record_key(1)).is_none(),
        "a branch's writes never reach its origin commit"
    );
}

/// A child sees its parent's state at fork time, and the parent does not see
/// the child's writes until they are merged.
fn fork_isolates_child_writes<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let parent = store.begin(None).unwrap();
    create_record(
        &mut store,
        parent,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let child = store.fork(parent).unwrap();
    create_record(&mut store, child, record_key(2), &[("n", u64_attribute(2))]);

    assert!(
        read_record(&store, ReadTarget::Branch(child), record_key(1)).is_some(),
        "child sees the parent's state as of the fork"
    );
    assert!(
        read_record(&store, ReadTarget::Branch(parent), record_key(2)).is_none(),
        "parent does not see the child's writes before merge"
    );
}

/// Discarding a child throws its writes away and leaves the parent untouched —
/// the revert half of a failed transaction.
fn discard_drops_child_writes<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let parent = store.begin(None).unwrap();
    let child = store.fork(parent).unwrap();
    create_record(&mut store, child, record_key(2), &[("n", u64_attribute(2))]);
    store.discard(child).unwrap();

    assert!(read_record(&store, ReadTarget::Branch(parent), record_key(2)).is_none());
}

/// Merging folds the child's diff into the parent and counts as exactly one
/// batch, however many writes the child absorbed.
fn merge_folds_child_into_parent<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let parent = store.begin(None).unwrap();
    let before = store.branch_info(parent).unwrap().version;

    let child = store.fork(parent).unwrap();
    create_record(&mut store, child, record_key(2), &[("n", u64_attribute(2))]);
    let after = store.merge(child).unwrap();

    assert_eq!(
        after.0,
        before.0 + 1,
        "a merged child counts as exactly one batch"
    );
    assert!(read_record(&store, ReadTarget::Branch(parent), record_key(2)).is_some());
}

/// A parent that advanced after the fork refuses the merge. In blockchain mode
/// this never fires — it asserts the sequential discipline.
fn merge_guard_fires_when_parent_advanced<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let parent = store.begin(None).unwrap();
    let child = store.fork(parent).unwrap();

    create_record(
        &mut store,
        parent,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert_eq!(store.merge(child).unwrap_err(), StoreError::Conflict);
}

/// A root branch whose origin is no longer head cannot commit. This is the
/// no-fork guarantee: a second block at one height is refused, and a host
/// should treat that as fatal rather than retry it.
fn commit_guard_fires_when_origin_not_head<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let first = store.begin(None).unwrap();
    let stale = store.begin(None).unwrap();

    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    store.commit(first).unwrap();

    create_record(&mut store, stale, record_key(2), &[("n", u64_attribute(2))]);
    assert_eq!(
        store.commit(stale).unwrap_err(),
        StoreError::Conflict,
        "a second block at one height must not commit"
    );
}

/// Handles are never reused: anything touching a consumed branch is refused.
fn consumed_handle_is_invalid<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    store.commit(branch).unwrap();

    assert_eq!(
        store.branch_info(branch).unwrap_err(),
        StoreError::HandleInvalid
    );
    assert_eq!(
        store
            .create(
                branch,
                record_key(1),
                cell_map(&[("n", u64_attribute(1))]),
                None
            )
            .unwrap_err(),
        StoreError::HandleInvalid
    );
}

/// Capability follows the constructor: only root branches commit, and a child
/// may merge or discard but never seal.
fn child_branch_cannot_commit<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let parent = store.begin(None).unwrap();
    let child = store.fork(parent).unwrap();

    assert_eq!(store.commit(child).unwrap_err(), StoreError::HandleInvalid);
}

/// Several root branches may be open over one commit at once, mutually
/// invisible — which is what lets a host validate competing payloads at one
/// height without any of them reaching the store.
fn concurrent_root_branches_are_independent<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let first = store.begin(None).unwrap();
    let second = store.begin(None).unwrap();

    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    create_record(
        &mut store,
        second,
        record_key(2),
        &[("n", u64_attribute(2))],
    );

    assert!(read_record(&store, ReadTarget::Branch(first), record_key(2)).is_none());
    assert!(read_record(&store, ReadTarget::Branch(second), record_key(1)).is_none());
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

/// Any mutation bumps the whole record's version by one.
fn patch_bumps_record_version<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let version = store
        .patch(
            branch,
            record_key(1),
            None,
            set_cell("n", u64_attribute(2)),
            None,
        )
        .unwrap()
        .into_value();
    assert_eq!(version, RecordVersion(2));
}

/// The optimistic-concurrency guard rejects a writer working from a stale read
/// and admits one whose version still matches.
fn patch_version_guard_rejects_stale_reader<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert_eq!(
        store
            .patch(
                branch,
                record_key(1),
                Some(RecordVersion(2)),
                set_cell("n", u64_attribute(9)),
                None
            )
            .unwrap_err(),
        StoreError::Conflict
    );
    assert!(
        store
            .patch(
                branch,
                record_key(1),
                Some(RecordVersion(1)),
                set_cell("n", u64_attribute(9)),
                None
            )
            .is_ok(),
        "the matching guard passes"
    );
}

/// An explicit guard of `0` always fails: versions start at 1, so it is
/// fail-closed rather than a don't-care sentinel.
fn zero_version_guard_never_matches<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert_eq!(
        store
            .patch(
                branch,
                record_key(1),
                Some(RecordVersion(0)),
                set_cell("n", u64_attribute(2)),
                None
            )
            .unwrap_err(),
        StoreError::Conflict,
        "0 is a guard that always fails, not a don't-care sentinel"
    );
}

/// Emptying a record by patch is refused — a record with no cells would be
/// indistinguishable from an absent one. Deleting is `delete`'s job.
fn patch_may_not_remove_last_cell<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert_eq!(
        store
            .patch(
                branch,
                record_key(1),
                None,
                vec![("n".to_string(), CellChange::Remove)],
                None
            )
            .unwrap_err(),
        StoreError::InvalidArgument
    );
}

/// Deleting removes the record; deleting again is `NotFound`, not a silent
/// success.
fn delete_removes_record<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    store.delete(branch, record_key(1), None, None).unwrap();

    assert!(read_record(&store, ReadTarget::Branch(branch), record_key(1)).is_none());
    assert_eq!(
        store.delete(branch, record_key(1), None, None).unwrap_err(),
        StoreError::NotFound
    );
}

/// `#` is reserved for store-internal meta entries and is refused in any
/// caller-supplied cell map.
fn reserved_cell_name_rejected<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();

    assert_eq!(
        store
            .create(
                branch,
                record_key(1),
                cell_map(&[("#version", u64_attribute(1))]),
                None
            )
            .unwrap_err(),
        StoreError::InvalidArgument,
        "`#` is reserved for store-internal meta entries"
    );
}

/// `bytes` has no index, so it may only be a field. The same value as a field
/// is accepted, which is what makes this a kind rule and not a type ban.
fn field_only_type_cannot_be_attribute<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();

    let indexed = Cell::attribute(TypeId::BYTES, vec![1, 2, 3]);
    assert_eq!(
        store
            .create(branch, record_key(1), cell_map(&[("blob", indexed)]), None)
            .unwrap_err(),
        StoreError::InvalidArgument
    );

    let plain = Cell::field(TypeId::BYTES, vec![1, 2, 3]);
    assert!(
        store
            .create(branch, record_key(1), cell_map(&[("blob", plain)]), None)
            .is_ok()
    );
}

/// Type id `0` is reserved and never storable, in either kind.
fn invalid_type_id_rejected<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    let reserved = Cell::field(TypeId(0), vec![1]);

    assert_eq!(
        store
            .create(branch, record_key(1), cell_map(&[("x", reserved)]), None)
            .unwrap_err(),
        StoreError::InvalidArgument,
        "type id 0 is reserved"
    );
}

// ---------------------------------------------------------------------------
// commitment
// ---------------------------------------------------------------------------

/// Record versions are coordination metadata, excluded from the digest: a
/// write that leaves content unchanged must not move it.
fn branch_hash_ignores_record_version<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let before = store.branch_hash(branch).unwrap();

    store
        .patch(
            branch,
            record_key(1),
            None,
            set_cell("n", u64_attribute(1)),
            None,
        )
        .unwrap();

    assert_eq!(
        store.branch_hash(branch).unwrap(),
        before,
        "versions are coordination metadata, excluded from the commitment"
    );
}

/// The digest is a pure function of content, not of history: it moves when
/// content moves, and returns when content returns.
fn branch_hash_tracks_content<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    let empty = store.branch_hash(branch).unwrap();

    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let one_record = store.branch_hash(branch).unwrap();
    assert_ne!(one_record, empty);

    store
        .patch(
            branch,
            record_key(1),
            None,
            set_cell("n", u64_attribute(2)),
            None,
        )
        .unwrap();
    assert_ne!(store.branch_hash(branch).unwrap(), one_record);

    store.delete(branch, record_key(1), None, None).unwrap();
    assert_eq!(
        store.branch_hash(branch).unwrap(),
        empty,
        "returning to the same content returns to the same digest"
    );
}

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

/// Queries answer from committed state, and `count` agrees with `query`.
fn query_sees_committed_state<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("kind", str_attribute("a"))],
    );
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("kind", str_attribute("b"))],
    );
    let committed = store.commit(branch).unwrap();

    let query = Query {
        filter: equals_filter("kind", TypeId::STR, b"a".to_vec()),
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };

    let found = store
        .query(Some(committed), &query, None)
        .unwrap()
        .into_value();
    assert_eq!(found.records.len(), 1);
    assert_eq!(found.records[0].key, record_key(1));

    assert_eq!(
        store
            .count(Some(committed), &query.filter, None)
            .unwrap()
            .into_value(),
        1,
        "count agrees with query"
    );
}

/// OR-groups union, and a record matching several groups comes back once.
fn query_dnf_unions_and_dedups<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("kind", str_attribute("a")), ("n", u64_attribute(1))],
    );
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("kind", str_attribute("b")), ("n", u64_attribute(2))],
    );
    let committed = store.commit(branch).unwrap();

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
    let query = Query {
        filter,
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };

    let found = store
        .query(Some(committed), &query, None)
        .unwrap()
        .into_value();
    assert_eq!(
        found.records.len(),
        1,
        "a record matching both groups is returned once"
    );
}

/// With no sort, results come back in ascending key order regardless of the
/// order they were written in.
fn query_defaults_to_key_order<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    for ordinal in [3u8, 1, 2] {
        create_record(
            &mut store,
            branch,
            record_key(ordinal),
            &[("kind", str_attribute("a"))],
        );
    }
    let committed = store.commit(branch).unwrap();

    let query = Query {
        filter: equals_filter("kind", TypeId::STR, b"a".to_vec()),
        page: Page {
            offset: 0,
            limit: 10,
        },
        ..Query::default()
    };

    let found = store
        .query(Some(committed), &query, None)
        .unwrap()
        .into_value();
    let keys: Vec<RecordKey> = found.records.iter().map(|record| record.key).collect();
    assert_eq!(keys, vec![record_key(1), record_key(2), record_key(3)]);
}

/// A range predicate against an equality-only type is refused, rather than
/// silently answered from an index that cannot support it.
fn query_range_on_eq_only_type_is_invalid<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("h", Cell::attribute(TypeId::BYTES32, vec![0u8; 32]))],
    );
    let committed = store.commit(branch).unwrap();

    let filter = Filter(vec![AndGroup(vec![Predicate {
        cell: "h".to_string(),
        op: CompareOp::Gt,
        type_id: TypeId::BYTES32,
        value: vec![0u8; 32],
        negated: false,
    }])]);

    assert_eq!(
        store.count(Some(committed), &filter, None).unwrap_err(),
        StoreError::InvalidQuery,
        "bytes32 indexes equality only"
    );
}

// ---------------------------------------------------------------------------
// genesis
// ---------------------------------------------------------------------------

/// The allocation lands as commit 1, leaving commit 0 the empty state — so
/// `head() >= 1` is the "genesis applied" marker, and block `n` is commit
/// `n + 1`.
fn genesis_lands_as_the_first_commit<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let committed = super::apply_genesis(
        &mut store,
        vec![(record_key(1), cell_map(&[("balance", u64_attribute(100))]))],
    )
    .expect("genesis applies to an empty store");

    assert_eq!(committed, CommitId::CHAIN_GENESIS);
    assert_eq!(store.head(), CommitId::CHAIN_GENESIS);
}

/// Applying genesis over live state is refused, so a restart cannot overwrite
/// the chain it is restarting.
fn genesis_refuses_to_apply_twice<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let allocation = || vec![(record_key(1), cell_map(&[("balance", u64_attribute(100))]))];

    super::apply_genesis(&mut store, allocation()).expect("the first apply succeeds");
    assert_eq!(
        super::apply_genesis(&mut store, allocation()).unwrap_err(),
        StoreError::AlreadyExists,
        "genesis is applied once or not at all"
    );
    assert_eq!(store.head(), CommitId::CHAIN_GENESIS, "and nothing moved");
}

/// Genesis records are ordinary committed state: readable at commit 1, absent
/// from the empty commit 0.
fn genesis_is_readable_at_its_commit<S: Store>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    super::apply_genesis(
        &mut store,
        vec![(record_key(1), cell_map(&[("balance", u64_attribute(100))]))],
    )
    .expect("genesis applies");

    let found = read_record(
        &store,
        ReadTarget::Commit(CommitId::CHAIN_GENESIS),
        record_key(1),
    )
    .expect("the genesis record is present at commit 1");
    assert_eq!(found.cell("balance"), Some(&u64_attribute(100)));
    assert!(
        read_record(&store, ReadTarget::Commit(CommitId::GENESIS), record_key(1)).is_none(),
        "commit 0 is the empty state"
    );
}

// ---------------------------------------------------------------------------
// extensions
// ---------------------------------------------------------------------------

/// A commit's host tag — for Arkiv, the block hash — resolves back to it, and
/// an unknown tag resolves to nothing.
fn commit_tag_round_trips<S: StoreExt>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let tag = [0xAB; 32];
    let committed = store.commit_tagged(branch, tag).unwrap();

    assert_eq!(store.commit_by_tag(tag).unwrap(), Some(committed));
    assert_eq!(store.commit_by_tag([0x00; 32]).unwrap(), None);
}

/// A commit's digest is the digest its branch carried at seal time, still
/// readable after the branch handle is gone.
fn commit_hash_matches_branch_hash<S: StoreExt>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let sealed = store.branch_hash(branch).unwrap();
    let committed = store.commit(branch).unwrap();

    assert_eq!(
        store.commit_hash(committed).unwrap(),
        sealed,
        "a commit's digest is the digest its branch had at seal time"
    );
}

/// Changesets report all three shapes — creation, mutation, deletion — as
/// before/after pairs, which is what pool maintenance and unwind both read.
fn changes_report_create_patch_delete<S: StoreExt>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();

    let creating = store.begin(None).unwrap();
    create_record(
        &mut store,
        creating,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let after_create = store.commit(creating).unwrap();

    let created = store.changes(after_create).unwrap();
    assert_eq!(created.len(), 1);
    assert!(created[0].before.is_none() && created[0].after.is_some());

    let patching = store.begin(None).unwrap();
    store
        .patch(
            patching,
            record_key(1),
            None,
            set_cell("n", u64_attribute(2)),
            None,
        )
        .unwrap();
    let after_patch = store.commit(patching).unwrap();

    let patched = store.changes(after_patch).unwrap();
    assert_eq!(patched.len(), 1);
    assert!(patched[0].before.is_some() && patched[0].after.is_some());

    let deleting = store.begin(None).unwrap();
    store.delete(deleting, record_key(1), None, None).unwrap();
    let after_delete = store.commit(deleting).unwrap();

    let deleted = store.changes(after_delete).unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].before.is_some() && deleted[0].after.is_none());
}

/// The same writes batched and applied one at a time reach the same digest.
/// This is what keeps batching a transport optimization and never a semantic
/// one — without it, the RPC binding could change consensus.
fn apply_batch_matches_individual_writes<S: StoreExt>(new_store: &dyn Fn() -> S) {
    let mut batched = new_store();
    let batched_branch = batched.begin(None).unwrap();
    batched
        .apply(
            batched_branch,
            vec![
                WriteOp::Create {
                    key: record_key(1),
                    cells: cell_map(&[("n", u64_attribute(1))]),
                },
                WriteOp::Create {
                    key: record_key(2),
                    cells: cell_map(&[("n", u64_attribute(2))]),
                },
                WriteOp::Patch {
                    key: record_key(1),
                    expected_version: None,
                    changes: set_cell("n", u64_attribute(9)),
                },
            ],
            None,
        )
        .unwrap();

    let mut serial = new_store();
    let serial_branch = serial.begin(None).unwrap();
    create_record(
        &mut serial,
        serial_branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    create_record(
        &mut serial,
        serial_branch,
        record_key(2),
        &[("n", u64_attribute(2))],
    );
    serial
        .patch(
            serial_branch,
            record_key(1),
            None,
            set_cell("n", u64_attribute(9)),
            None,
        )
        .unwrap();

    assert_eq!(
        batched.branch_hash(batched_branch).unwrap(),
        serial.branch_hash(serial_branch).unwrap()
    );
}

/// A batched read answers exactly what the same keys read one at a time would,
/// including the misses and their positions.
fn get_many_matches_individual_gets<S: StoreExt>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let keys = [record_key(1), record_key(2)];
    let together = store
        .get_many(ReadTarget::Branch(branch), &keys, None, None)
        .unwrap()
        .into_value();
    let separately: Vec<Option<Record>> = keys
        .iter()
        .map(|key| read_record(&store, ReadTarget::Branch(branch), *key))
        .collect();

    assert_eq!(together, separately);
}

/// The retention window always includes the head, and is well-ordered.
fn retention_covers_head<S: StoreExt>(new_store: &dyn Fn() -> S) {
    let mut store = new_store();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let committed = store.commit(branch).unwrap();

    let (oldest, newest) = store.retention();
    assert_eq!(newest, committed);
    assert!(oldest <= newest);
}
