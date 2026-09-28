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
pub fn run_all<S: Store>(store_generator: &dyn Fn() -> S) {
    new_store_starts_at_commit_zero(store_generator);
    read_write_on_branch_without_commit_works(store_generator);
    read_patch_write_on_branch_without_commit_works(store_generator);
    absent_record_read_returns_none(store_generator);
    create_recordid_collision_is_failure(store_generator);
    commit_advances_head_by_exactly_one(store_generator);
    committed_state_is_readable_at_commit(store_generator);
    branch_writes_invisible_to_parent_commit(store_generator);
    branch_drop_invisible_to_parent_commit(store_generator);
    merge_folds_child_branch_into_parent_branch(store_generator);
    merge_guard_fires_when_parent_advanced(store_generator);
    commit_guard_rejects_second_branch_committal(store_generator);
    committed_branchid_becomes_invalid(store_generator);
    nested_branch_cannot_commit_to_grandparent(store_generator);
    concurrent_branches_have_independent_views(store_generator);
    patch_bumps_record_version(store_generator);
    patch_version_guard_rejects_patch_on_state_view(store_generator);
    recordversion_zero_version_guard_always_fails(store_generator);
    patch_may_not_remove_last_cell(store_generator); // I do not understand this
    delete_removes_record(store_generator);
    reserved_cell_name_rejected(store_generator);
    field_only_type_cannot_be_attribute(store_generator); // I do not understand this
    invalid_type_id_rejected(store_generator);
    branch_digest_doesnt_include_record_version(store_generator);
    branch_digest_tracks_content(store_generator);
    query_sees_committed_state(store_generator);
    query_dnf_unions_and_dedups(store_generator);
    query_defaults_to_key_order(store_generator);
    query_range_on_eq_only_type_is_invalid(store_generator);
    genesis_lands_as_the_first_commit(store_generator);
    genesis_refuses_to_apply_twice(store_generator);
    genesis_is_readable_at_its_commit(store_generator);
    genesis_is_all_or_nothing(store_generator);
    block_lifecycle_at_depth_three(store_generator);
    discard_drops_descendants(store_generator);
    branch_reads_its_origin_not_the_head(store_generator);
    projection_limits_returned_cells(store_generator);
    query_and_group_intersects_predicates(store_generator);
    query_negated_predicate_excludes_matches(store_generator);
    query_pages_with_offset_and_limit(store_generator);
    query_reports_total_matched_beyond_the_page(store_generator);
    query_sorts_ascending_and_descending(store_generator);
    query_sort_puts_records_without_the_cell_last(store_generator);
}

/// Run every [`StoreExt`] assertion against stores built by `new_store`.
///
/// Separate from [`run_all`] because the extensions are additions to
/// `golem-db-api.md`, not part of it — an implementation may legitimately
/// conform to the spec and not yet implement these.
pub fn run_all_ext<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    commits_are_resolvable_by_their_committag(store_generator);
    digest_same_on_branch_committal(store_generator);
    changeset_report_all_crud_ops(store_generator);
    apply_batch_matches_individual_writes(store_generator);
    batch_ops_and_individual_ops_are_indistinguishable(store_generator);
    retention_window_always_covers_HEAD(store_generator);
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
fn new_store_starts_at_commit_zero<S: Store>(store_generator: &dyn Fn() -> S) {
    assert_eq!(store_generator().head(), CommitId::GENESIS);
}

/// A created record reads back with its key, its cells, and version 1.
fn read_write_on_branch_without_commit_works<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn read_patch_write_on_branch_without_commit_works<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn absent_record_read_returns_none<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    assert!(read_record(&store, ReadTarget::Branch(branch), record_key(99)).is_none());
}

/// Creating over an existing key is refused; `create` never overwrites.
fn create_recordid_collision_is_failure<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn commit_advances_head_by_exactly_one<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn committed_state_is_readable_at_commit<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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

/// A child sees its parent's state at fork time, and the parent does not see
/// the child's writes until they are merged.
fn branch_writes_invisible_to_parent_commit<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn branch_drop_invisible_to_parent_commit<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let parent = store.begin(None).unwrap();
    let child = store.fork(parent).unwrap();
    create_record(&mut store, child, record_key(2), &[("n", u64_attribute(2))]);
    store.discard(child).unwrap();

    assert!(read_record(&store, ReadTarget::Branch(parent), record_key(2)).is_none());
}

/// Merging folds the child's diff into the parent and counts as exactly one
/// batch, however many writes the child absorbed.
fn merge_folds_child_branch_into_parent_branch<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn merge_guard_fires_when_parent_advanced<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn commit_guard_rejects_second_branch_committal<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn committed_branchid_becomes_invalid<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
/// may merge or discard but never promote.
fn nested_branch_cannot_commit_to_grandparent<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let parent = store.begin(None).unwrap();
    let child = store.fork(parent).unwrap();

    assert_eq!(store.commit(child).unwrap_err(), StoreError::HandleInvalid);
}

/// Several root branches may be open over one commit at once, mutually
/// invisible — which is what lets a host validate competing payloads at one
/// height without any of them reaching the store.
fn concurrent_branches_have_independent_views<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn patch_bumps_record_version<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn patch_version_guard_rejects_patch_on_state_view<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn recordversion_zero_version_guard_always_fails<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn patch_may_not_remove_last_cell<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn delete_removes_record<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn reserved_cell_name_rejected<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn field_only_type_cannot_be_attribute<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn invalid_type_id_rejected<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn branch_digest_doesnt_include_record_version<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let before = store.branch_digest(branch).unwrap();

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
        store.branch_digest(branch).unwrap(),
        before,
        "versions are coordination metadata, excluded from the commitment"
    );
}

/// The digest is a pure function of content, not of history: it moves when
/// content moves, and returns when content returns.
fn branch_digest_tracks_content<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    let empty = store.branch_digest(branch).unwrap();

    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    let one_record = store.branch_digest(branch).unwrap();
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
    assert_ne!(store.branch_digest(branch).unwrap(), one_record);

    store.delete(branch, record_key(1), None, None).unwrap();
    assert_eq!(
        store.branch_digest(branch).unwrap(),
        empty,
        "returning to the same content returns to the same digest"
    );
}

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

/// Queries answer from committed state, and `count` agrees with `query`.
fn query_sees_committed_state<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn query_dnf_unions_and_dedups<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn query_defaults_to_key_order<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn query_range_on_eq_only_type_is_invalid<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn genesis_lands_as_the_first_commit<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn genesis_refuses_to_apply_twice<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn genesis_is_readable_at_its_commit<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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

/// A genesis that fails part-way leaves **nothing** behind: head stays at the
/// empty commit, and no record from the accepted prefix is readable.
///
/// Without this, a rejected allocation could leave a half-populated commit 1
/// that is neither empty nor genesis — and since `apply_genesis` refuses once
/// `head != 0`, the chain could never be repaired.
fn genesis_is_all_or_nothing<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let outcome = super::apply_genesis(
        &mut store,
        vec![
            (record_key(1), cell_map(&[("balance", u64_attribute(100))])),
            // `#` is reserved, so this entry is refused — after the first was
            // already staged.
            (record_key(2), cell_map(&[("#version", u64_attribute(1))])),
        ],
    );

    assert_eq!(outcome.unwrap_err(), StoreError::InvalidArgument);
    assert_eq!(
        store.head(),
        CommitId::GENESIS,
        "a failed genesis commits nothing"
    );
    assert!(
        read_record(&store, ReadTarget::Commit(CommitId::GENESIS), record_key(1)).is_none(),
        "not even the entries that were accepted before the failure"
    );
}

/// The block lifecycle the blockchain profile specifies, end to end: a block
/// branch, a transaction frame per transaction, and an op-batch frame inside
/// each one.
///
/// This is the assertion that matters most, because it is the only sequence
/// Arkiv actually runs. The load-bearing rule is the **two rollback scopes**: a
/// reverted transaction loses its entity ops but keeps its fee accounting, so
/// the op frame is discarded while the transaction frame is merged either way.
fn block_lifecycle_at_depth_three<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let block = store.begin(None).unwrap();

    // Transaction one succeeds: fee frame and op frame both land.
    let succeeding = store.fork(block).unwrap();
    create_record(
        &mut store,
        succeeding,
        record_key(1),
        &[("fee", u64_attribute(21))],
    );
    let ops = store.fork(succeeding).unwrap();
    create_record(
        &mut store,
        ops,
        record_key(10),
        &[("entity", u64_attribute(1))],
    );
    store.merge(ops).unwrap();
    store.merge(succeeding).unwrap();

    // Transaction two reverts: the op frame is discarded, the fee frame is
    // merged regardless.
    let reverting = store.fork(block).unwrap();
    create_record(
        &mut store,
        reverting,
        record_key(2),
        &[("fee", u64_attribute(21))],
    );
    let doomed = store.fork(reverting).unwrap();
    create_record(
        &mut store,
        doomed,
        record_key(20),
        &[("entity", u64_attribute(2))],
    );
    store.discard(doomed).unwrap();
    store.merge(reverting).unwrap();

    let committed = store.commit(block).unwrap();
    let present = |key| read_record(&store, ReadTarget::Commit(committed), key).is_some();

    assert!(present(record_key(1)), "the successful tx's fee");
    assert!(present(record_key(10)), "and its entity writes");
    assert!(
        present(record_key(2)),
        "the reverted tx still paid: fee accounting survives failure"
    );
    assert!(
        !present(record_key(20)),
        "but its entity writes were discarded"
    );
}

/// Discarding a branch takes its open descendants with it: they are grounded on
/// a handle that no longer exists, so continuing to use them would read from a
/// parent that is gone.
fn discard_drops_descendants<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let block = store.begin(None).unwrap();
    let transaction = store.fork(block).unwrap();
    let ops = store.fork(transaction).unwrap();

    store.discard(transaction).unwrap();

    assert_eq!(
        store.branch_info(ops).unwrap_err(),
        StoreError::HandleInvalid,
        "the grandchild went with its parent"
    );
    assert!(
        store.branch_info(block).is_ok(),
        "but the grandparent is untouched"
    );
}

/// A branch reads the commit it was opened on, not whatever head has since
/// become — which is what makes a branch a stable base for simulation while
/// the chain advances underneath it.
fn branch_reads_its_origin_not_the_head<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();

    let first = store.begin(None).unwrap();
    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    let origin = store.commit(first).unwrap();

    // Opened on `origin`, and held open while the chain moves on.
    let held = store.begin(None).unwrap();

    let second = store.begin(None).unwrap();
    create_record(
        &mut store,
        second,
        record_key(2),
        &[("n", u64_attribute(2))],
    );
    let newer = store.commit(second).unwrap();
    assert_ne!(newer, origin, "head advanced past the held branch's origin");

    assert!(
        read_record(&store, ReadTarget::Branch(held), record_key(1)).is_some(),
        "the held branch still sees its origin's state"
    );
    assert!(
        read_record(&store, ReadTarget::Branch(held), record_key(2)).is_none(),
        "and does not see a commit made after it was opened"
    );
}

/// A projection returns only the named cells. The record's key and version are
/// always present — they identify it, they are not content.
fn projection_limits_returned_cells<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("wanted", u64_attribute(1)), ("unwanted", u64_attribute(2))],
    );

    let projected = store
        .get(
            ReadTarget::Branch(branch),
            record_key(1),
            Some(&["wanted".to_string()]),
            None,
        )
        .unwrap()
        .into_value()
        .expect("the record is present");

    assert_eq!(projected.key, record_key(1));
    assert_eq!(projected.cell("wanted"), Some(&u64_attribute(1)));
    assert_eq!(projected.cell("unwanted"), None, "projected away");
}

// ---------------------------------------------------------------------------
// query: grouping, paging, ordering
// ---------------------------------------------------------------------------

/// Build a committed three-record fixture: keys 1..=3, `kind` and `n` cells.
fn committed_fixture<S: Store>(store: &mut S) -> CommitId {
    let branch = store.begin(None).unwrap();
    for (ordinal, kind) in [(1u8, "a"), (2, "a"), (3, "b")] {
        create_record(
            store,
            branch,
            record_key(ordinal),
            &[
                ("kind", str_attribute(kind)),
                ("n", u64_attribute(u64::from(ordinal))),
            ],
        );
    }
    store.commit(branch).unwrap()
}

fn paged_query(filter: Filter, offset: u64, limit: u64) -> Query {
    Query {
        filter,
        page: Page { offset, limit },
        ..Query::default()
    }
}

fn matched_keys<S: Store>(store: &S, at: CommitId, query: &Query) -> Vec<RecordKey> {
    store
        .query(Some(at), query, None)
        .unwrap()
        .into_value()
        .records
        .iter()
        .map(|record| record.key)
        .collect()
}

/// Predicates inside one AND-group intersect: a record must satisfy all of
/// them, not merely one.
fn query_and_group_intersects_predicates<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);

    let both = Filter(vec![AndGroup(vec![
        Predicate {
            cell: "kind".to_string(),
            op: CompareOp::Eq,
            type_id: TypeId::STR,
            value: b"a".to_vec(),
            negated: false,
        },
        Predicate {
            cell: "n".to_string(),
            op: CompareOp::Eq,
            type_id: TypeId::U64,
            value: 2u64.to_be_bytes().to_vec(),
            negated: false,
        },
    ])]);

    assert_eq!(
        matched_keys(&store, at, &paged_query(both, 0, 10)),
        vec![record_key(2)],
        "only the record satisfying both predicates"
    );
}

/// A negated predicate matches the records the predicate does not.
fn query_negated_predicate_excludes_matches<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);

    let not_a = Filter(vec![AndGroup(vec![Predicate {
        cell: "kind".to_string(),
        op: CompareOp::Eq,
        type_id: TypeId::STR,
        value: b"a".to_vec(),
        negated: true,
    }])]);

    assert_eq!(
        matched_keys(&store, at, &paged_query(not_a, 0, 10)),
        vec![record_key(3)],
        "the one record whose kind is not `a`"
    );
}

/// `offset` skips and `limit` truncates, over the result order.
fn query_pages_with_offset_and_limit<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);
    let all = || Filter::default();

    assert_eq!(
        matched_keys(&store, at, &paged_query(all(), 0, 2)),
        vec![record_key(1), record_key(2)],
        "limit truncates"
    );
    assert_eq!(
        matched_keys(&store, at, &paged_query(all(), 1, 1)),
        vec![record_key(2)],
        "offset skips"
    );
    assert!(
        matched_keys(&store, at, &paged_query(all(), 99, 10)).is_empty(),
        "an offset past the end is an empty page, not an error"
    );
}

/// `total_matched` counts the whole match, not the page — so a caller can page
/// without losing the size of what it is paging through.
fn query_reports_total_matched_beyond_the_page<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);

    let counted = Query {
        total_matched: true,
        ..paged_query(Filter::default(), 0, 1)
    };
    let result = store.query(Some(at), &counted, None).unwrap().into_value();

    assert_eq!(result.records.len(), 1, "one record on the page");
    assert_eq!(result.total_matched, Some(3), "three in the match");

    let uncounted = paged_query(Filter::default(), 0, 1);
    assert_eq!(
        store
            .query(Some(at), &uncounted, None)
            .unwrap()
            .into_value()
            .total_matched,
        None,
        "and it is absent unless asked for"
    );
}

/// Sorting orders by a cell in either direction.
fn query_sorts_ascending_and_descending<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);

    let sorted = |direction| Query {
        sort: Some(Sort {
            cell: "n".to_string(),
            direction,
        }),
        ..paged_query(Filter::default(), 0, 10)
    };

    assert_eq!(
        matched_keys(&store, at, &sorted(SortDirection::Ascending)),
        vec![record_key(1), record_key(2), record_key(3)]
    );
    assert_eq!(
        matched_keys(&store, at, &sorted(SortDirection::Descending)),
        vec![record_key(3), record_key(2), record_key(1)]
    );
}

/// A record lacking the sort cell sorts last, and ties break on key order — so
/// the ordering is total and two implementations agreeing on the match set also
/// agree on the page.
fn query_sort_puts_records_without_the_cell_last<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    // Two records share a sort value, and one has no sort cell at all.
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("n", u64_attribute(1))],
    );
    create_record(
        &mut store,
        branch,
        record_key(3),
        &[("other", u64_attribute(9))],
    );
    let at = store.commit(branch).unwrap();

    let query = Query {
        sort: Some(Sort {
            cell: "n".to_string(),
            direction: SortDirection::Ascending,
        }),
        ..paged_query(Filter::default(), 0, 10)
    };

    assert_eq!(
        matched_keys(&store, at, &query),
        vec![record_key(1), record_key(2), record_key(3)],
        "tie broken by key, and the record without the cell last"
    );
}

// ---------------------------------------------------------------------------
// extensions
// ---------------------------------------------------------------------------

/// A commit's host tag — for Arkiv, the block hash — resolves back to it, and
/// an unknown tag resolves to nothing.
fn commits_are_resolvable_by_their_committag<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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

/// A commit's digest is the digest its branch carried at commit time, still
/// readable after the branch handle is gone.
fn digest_same_on_branch_committal<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let at_commit_time = store.branch_digest(branch).unwrap();
    let committed = store.commit(branch).unwrap();

    assert_eq!(
        store.commit_hash(committed).unwrap(),
        at_commit_time,
        "a commit's digest is the digest its branch had at commit time"
    );
}

/// Changesets report all three shapes — creation, mutation, deletion — as
/// before/after pairs, which is what pool maintenance and unwind both read.
fn changeset_report_all_crud_ops<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();

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
fn apply_batch_matches_individual_writes<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut batched = store_generator();
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

    let mut serial = store_generator();
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
        batched.branch_digest(batched_branch).unwrap(),
        serial.branch_digest(serial_branch).unwrap()
    );
}

/// A batched read answers exactly what the same keys read one at a time would,
/// including the misses and their positions.
fn batch_ops_and_individual_ops_are_indistinguishable<S: StoreExt>(
    store_generator: &dyn Fn() -> S,
) {
    let mut store = store_generator();
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
#[allow(non_snake_case)]
fn retention_window_always_covers_HEAD<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
