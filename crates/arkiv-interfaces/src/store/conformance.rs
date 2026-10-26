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
    branch_writes_invisible_to_other_branches(store_generator);
    branch_drop_leaves_no_trace(store_generator);
    rollback_undoes_the_open_frame(store_generator);
    rollback_is_not_idempotent(store_generator);
    rollback_past_the_first_frame_is_refused(store_generator);
    seal_computes_roots_without_persisting(store_generator);
    seal_is_idempotent(store_generator);
    a_sealed_branch_refuses_writes(store_generator);
    two_branches_over_one_head_seal_independently(store_generator);
    commit_seals_if_the_host_did_not(store_generator);
    commit_guard_rejects_second_branch_committal(store_generator);
    committed_branchid_becomes_invalid(store_generator);
    concurrent_branches_have_independent_views(store_generator);
    patch_bumps_record_version(store_generator);
    patch_version_guard_rejects_patch_on_state_view(store_generator);
    recordversion_zero_version_guard_always_fails(store_generator);
    patch_may_not_remove_last_cell(store_generator); // I do not understand this
    patch_set_adds_cells_and_takes_their_kind(store_generator);
    delete_removes_record(store_generator);
    reserved_cell_name_rejected(store_generator);
    field_only_type_cannot_be_attribute(store_generator); // I do not understand this
    invalid_type_id_rejected(store_generator);
    branch_hash_doesnt_include_record_version(store_generator);
    branch_hash_tracks_content(store_generator);
    query_sees_committed_state(store_generator);
    query_or_unions_and_dedups(store_generator);
    query_without_a_sort_pages_in_a_fixed_order(store_generator);
    query_range_follows_each_types_order(store_generator);
    query_prefix_on_a_type_without_one_is_invalid(store_generator);
    genesis_lands_as_the_first_commit(store_generator);
    genesis_refuses_to_apply_twice(store_generator);
    genesis_is_readable_at_its_commit(store_generator);
    genesis_is_all_or_nothing(store_generator);
    block_lifecycle_in_frames(store_generator);
    branch_reads_its_origin_not_the_head(store_generator);
    projection_limits_returned_cells(store_generator);
    query_and_intersects_its_children(store_generator);
    query_not_excludes_matches(store_generator);
    query_presence_matches_records_carrying_the_attribute(store_generator);
    query_pages_with_offset_and_limit(store_generator);
    query_reports_total_matched_beyond_the_page(store_generator);
    query_sorts_ascending_and_descending(store_generator);
    query_sort_puts_records_without_the_cell_last(store_generator);
    failed_commit_changes_nothing(store_generator);
    failed_commit_leaves_the_branch_usable(store_generator);
    commit_applies_every_record_or_none(store_generator);
}

/// Run every [`StoreExt`] assertion against stores built by `new_store`.
///
/// Separate from [`run_all`] because the extensions are additions to
/// `golem-db-api.md`, not part of it — an implementation may legitimately
/// conform to the spec and not yet implement these.
pub fn run_all_ext<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    changes_account_for_the_whole_difference(store_generator);
    failed_commit_writes_no_changeset(store_generator);
    commits_are_resolvable_by_their_committag(store_generator);
    a_commits_tag_is_readable_from_the_commit(store_generator);
    an_untagged_commit_has_no_tag(store_generator);
    digest_same_on_branch_committal(store_generator);
    changeset_report_all_crud_ops(store_generator);
    apply_batch_matches_individual_writes(store_generator);
    batch_ops_and_individual_ops_are_indistinguishable(store_generator);
    retention_window_always_covers_HEAD(store_generator);
}

/// Run every segment assertion against stores built by `new_store`.
///
/// Separate again, and for a different reason from [`run_all_ext`]: segment
/// storage *is* specified upstream, but it is unimplemented there today and
/// the trait methods default to [`StoreError::NotImplemented`]. A store that
/// has not built the tier yet is still a conforming store; one that has is
/// held to all of this.
pub fn run_all_segments<S: Store>(store_generator: &dyn Fn() -> S) {
    appending_before_seal_is_refused(store_generator);
    an_appended_row_is_invisible_until_commit(store_generator);
    a_committed_row_reads_back_by_ordinal(store_generator);
    a_committed_row_reads_back_by_key(store_generator);
    ordinals_are_dense_and_in_append_order(store_generator);
    a_duplicate_key_is_refused_and_overwrites_nothing(store_generator);
    a_discarded_branchs_rows_never_appear(store_generator);
    range_of_reports_what_one_commit_appended(store_generator);
    rows_of_returns_that_commits_rows_in_order(store_generator);
    an_untouched_segment_is_empty_not_an_error(store_generator);
    segments_are_independent_of_each_other(store_generator);
    a_row_needs_no_key(store_generator);
    a_batch_append_matches_one_at_a_time(store_generator);
    a_batch_with_a_duplicate_name_is_refused(store_generator);
    a_name_resolves_to_the_ordinal_it_was_appended_at(store_generator);
    an_empty_range_still_says_where_the_commit_stood(store_generator);
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

/// One predicate: `cell == value`.
fn equals_filter(cell: &str, type_id: TypeId, value: Vec<u8>) -> Filter {
    Filter::Predicate(Predicate {
        cell: cell.to_string(),
        test: Test::Compare {
            op: CompareOp::Eq,
            type_id,
            value,
        },
    })
}

/// The digest of committed state, using only [`Store`].
///
/// A fresh branch over head has staged nothing, so its digest *is* the
/// committed one — which is how the atomicity assertions observe the digest
/// without needing [`StoreExt::commit_hash`].
fn committed_digest<S: Store>(store: &mut S) -> [u8; 32] {
    let probe = store.begin(None).expect("open a probe branch");
    let digest = store.branch_hash(probe).expect("read its digest");
    store.discard(probe).expect("and drop it again");
    digest
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
    let store = store_generator();
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

/// Branches over one head are independent: neither sees the other's writes.
/// This is what lets a host validate competing payloads at one height.
fn branch_writes_invisible_to_other_branches<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let a = store.begin(None).unwrap();
    let b = store.begin(None).unwrap();

    create_record(&mut store, a, record_key(1), &[("n", u64_attribute(1))]);
    create_record(&mut store, b, record_key(2), &[("n", u64_attribute(2))]);

    assert!(read_record(&store, ReadTarget::Branch(a), record_key(2)).is_none());
    assert!(read_record(&store, ReadTarget::Branch(b), record_key(1)).is_none());
}

/// Discarding a branch throws its writes away and leaves the commit untouched.
fn branch_drop_leaves_no_trace<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let head = store.head();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("n", u64_attribute(2))],
    );
    store.discard(branch).unwrap();

    assert_eq!(store.head(), head, "a discard does not move head");
    assert!(read_record(&store, ReadTarget::Commit(head), record_key(2)).is_none());
}

/// `rollback` undoes everything written since the last checkpoint — the revert
/// half of a failed operation.
fn rollback_undoes_the_open_frame<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    store.checkpoint(branch).unwrap();

    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("n", u64_attribute(2))],
    );
    store.rollback(branch).unwrap();

    assert!(
        read_record(&store, ReadTarget::Branch(branch), record_key(1)).is_some(),
        "the checkpointed frame survives"
    );
    assert!(
        read_record(&store, ReadTarget::Branch(branch), record_key(2)).is_none(),
        "the open frame is undone"
    );
}

/// **Not idempotent**: calling it twice undoes two frames.
fn rollback_is_not_idempotent<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    store.checkpoint(branch).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("n", u64_attribute(2))],
    );
    store.checkpoint(branch).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(3),
        &[("n", u64_attribute(3))],
    );

    store.rollback(branch).unwrap();
    store.rollback(branch).unwrap();

    assert!(read_record(&store, ReadTarget::Branch(branch), record_key(1)).is_some());
    assert!(
        read_record(&store, ReadTarget::Branch(branch), record_key(2)).is_none(),
        "two rollbacks undo two frames"
    );
    assert!(read_record(&store, ReadTarget::Branch(branch), record_key(3)).is_none());
}

/// `begin` opens the first frame, so there is exactly one more frame to roll
/// back than the host checkpointed. Past that is the branch itself.
fn rollback_past_the_first_frame_is_refused<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    store.rollback(branch).expect("the first frame rolls back");
    assert!(read_record(&store, ReadTarget::Branch(branch), record_key(1)).is_none());
    assert_eq!(
        store.rollback(branch).unwrap_err(),
        StoreError::HandleInvalid,
        "there is nothing left to roll back"
    );
}

/// The half of a commit a host needs before it knows whether the block will be
/// adopted: roots computed, nothing written.
fn seal_computes_roots_without_persisting<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let head = store.head();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let sealed = store.seal(branch).expect("seal");
    assert_eq!(sealed.commit_nr.0, head.0 + 1, "the commit it would take");
    assert_eq!(store.head(), head, "but head has not moved");
    assert!(
        read_record(&store, ReadTarget::Commit(head), record_key(1)).is_none(),
        "and nothing was written"
    );
    assert!(
        read_record(&store, ReadTarget::Branch(branch), record_key(1)).is_some(),
        "a sealed branch is still readable"
    );
}

/// Sealing twice is the same freeze, so a host that seals defensively and then
/// commits gets one answer.
fn seal_is_idempotent<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    assert_eq!(store.seal(branch).unwrap(), store.seal(branch).unwrap());
}

fn a_sealed_branch_refuses_writes<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = store.begin(None).unwrap();
    store.seal(branch).unwrap();

    assert_eq!(
        store
            .create(
                branch,
                record_key(1),
                vec![(CellName::from("n"), u64_attribute(1))],
                None,
            )
            .unwrap_err(),
        StoreError::HandleInvalid,
    );
}

/// The property a host builds blocks on: any number of branches may be sealed
/// over one head, and whichever is adopted commits while the rest leave no
/// trace. Two branches with the same writes seal to the same roots, which is
/// what makes building a block and re-validating it agree.
fn two_branches_over_one_head_seal_independently<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let head = store.head();

    let a = store.begin(None).unwrap();
    create_record(&mut store, a, record_key(1), &[("n", u64_attribute(1))]);
    let sealed_a = store.seal(a).unwrap();

    let b = store.begin(None).unwrap();
    create_record(&mut store, b, record_key(1), &[("n", u64_attribute(1))]);
    let sealed_b = store.seal(b).unwrap();

    assert_eq!(
        sealed_a, sealed_b,
        "the same writes over the same head seal to the same roots"
    );
    assert_eq!(store.head(), head, "neither seal moved head");

    store.commit(a).expect("one is adopted");
    store.discard(b).expect("the other leaves no trace");
    assert_eq!(store.head().0, head.0 + 1);
}

/// "Implies a final checkpoint, and a seal if none was taken."
fn commit_seals_if_the_host_did_not<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let head = store.head();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let commit = store
        .commit(branch)
        .expect("commit without an explicit seal");
    assert_eq!(commit.0, head.0 + 1);
    assert!(read_record(&store, ReadTarget::Commit(commit), record_key(1)).is_some());
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
    let store = store_generator();
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

/// `Set` writes the cell whether or not the record holds it yet, with the kind
/// it names, and queries follow that kind. Arkiv's account and entity writes
/// patch in cells a record lacks, and an update can make an attribute indexed:
/// a store whose `Set` only replaced existing cells, or kept their old kind,
/// would fail both.
fn patch_set_adds_cells_and_takes_their_kind<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("tag", Cell::field(TypeId::STR, b"x".to_vec()))],
    );
    store
        .patch(
            branch,
            record_key(1),
            None,
            vec![
                ("tag".to_string(), CellChange::Set(str_attribute("y"))),
                ("n".to_string(), CellChange::Set(u64_attribute(7))),
            ],
            None,
        )
        .unwrap();

    let found = read_record(&store, ReadTarget::Branch(branch), record_key(1)).unwrap();
    assert_eq!(
        found.cell("tag"),
        Some(&str_attribute("y")),
        "the kind it names"
    );
    assert_eq!(found.cell("n"), Some(&u64_attribute(7)), "a cell it lacked");

    let at = store.commit(branch).unwrap();
    let tagged = paged_query(equals_filter("tag", TypeId::STR, b"y".to_vec()), 0, 10);
    assert_eq!(
        matched_keys(&store, at, &tagged),
        vec![record_key(1)],
        "an attribute now, so a query finds it"
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
    let store = store_generator();
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
    let store = store_generator();
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
    let store = store_generator();
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
fn branch_hash_doesnt_include_record_version<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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
fn branch_hash_tracks_content<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
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

/// An `Or` unions its children, and a record several of them match comes back
/// once.
fn query_or_unions_and_dedups<S: Store>(store_generator: &dyn Fn() -> S) {
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

    let filter = Filter::Or(vec![
        equals_filter("kind", TypeId::STR, b"a".to_vec()),
        equals_filter("n", TypeId::U64, 1u64.to_be_bytes().to_vec()),
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
        "a record matching both children is returned once"
    );
}

/// With no sort, results follow the store's own order, which need not be key
/// order — GolemDB's is the order records were created in — but is fixed for
/// a commit: the same query lists the same records in the same order, so pages
/// taken by offset partition the matches. Written out of key order, so a store
/// that lists in one of those orders and pages in the other would show it.
fn query_without_a_sort_pages_in_a_fixed_order<S: Store>(store_generator: &dyn Fn() -> S) {
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
    let page = |offset, limit| {
        let filter = equals_filter("kind", TypeId::STR, b"a".to_vec());
        matched_keys(&store, committed, &paged_query(filter, offset, limit))
    };

    let all = page(0, 10);
    let mut every_match = all.clone();
    every_match.sort_unstable();
    assert_eq!(
        every_match,
        vec![record_key(1), record_key(2), record_key(3)],
        "every match, once"
    );
    assert_eq!(page(0, 10), all, "the same query lists the same order");
    let walked: Vec<RecordKey> = (0..3).flat_map(|offset| page(offset, 1)).collect();
    assert_eq!(walked, all, "pages partition the matches, in that order");
}

/// A range predicate follows its type's order: `false < true` for a `bool`,
/// and unsigned byte order for a `str`, `bytes32`, `addr` or `key`, where a
/// shorter `str` sorts before a longer one it is a prefix of.
fn query_range_follows_each_types_order<S: Store>(store_generator: &dyn Fn() -> S) {
    // A fixed-width value starting with `first`: `0x80` must sort above
    // `0x7f`, which no sign bias would leave it.
    let word = |first: u8, len: usize| {
        let mut bytes = vec![0u8; len];
        bytes[0] = first;
        bytes
    };

    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    for (ordinal, (flag, text, first)) in [
        (1u8, (0u8, "a", 0x00u8)),
        (2, (1, "ab", 0x7f)),
        (3, (1, "b", 0x80)),
    ] {
        create_record(
            &mut store,
            branch,
            record_key(ordinal),
            &[
                ("f", Cell::attribute(TypeId::BOOL, vec![flag])),
                ("s", str_attribute(text)),
                ("h", Cell::attribute(TypeId::BYTES32, word(first, 32))),
                ("a", Cell::attribute(TypeId::ADDR, word(first, 20))),
                ("k", Cell::attribute(TypeId::KEY, word(first, 32))),
            ],
        );
    }
    let at = store.commit(branch).unwrap();

    let compare = |cell: &str, op, type_id, value: Vec<u8>| {
        let filter = Filter::Predicate(Predicate {
            cell: cell.to_string(),
            test: Test::Compare { op, type_id, value },
        });
        matched_keys(&store, at, &paged_query(filter, 0, 10))
    };

    assert_eq!(
        compare("f", CompareOp::Gt, TypeId::BOOL, vec![0]),
        vec![record_key(2), record_key(3)],
        "false < true"
    );
    assert_eq!(
        compare("s", CompareOp::Gt, TypeId::STR, b"a".to_vec()),
        vec![record_key(2), record_key(3)],
        "a prefix sorts before the strings it starts"
    );
    assert_eq!(
        compare("s", CompareOp::Lte, TypeId::STR, b"ab".to_vec()),
        vec![record_key(1), record_key(2)],
    );
    for (cell, type_id, len) in [
        ("h", TypeId::BYTES32, 32),
        ("a", TypeId::ADDR, 20),
        ("k", TypeId::KEY, 32),
    ] {
        assert_eq!(
            compare(cell, CompareOp::Gte, type_id, word(0x7f, len)),
            vec![record_key(2), record_key(3)],
            "{type_id:?} orders as unsigned bytes"
        );
        assert_eq!(
            compare(cell, CompareOp::Lt, type_id, word(0x80, len)),
            vec![record_key(1), record_key(2)],
            "{type_id:?} orders as unsigned bytes"
        );
    }
}

/// A prefix predicate against any type but `str` is refused, rather than
/// silently answered from an index that cannot support it.
fn query_prefix_on_a_type_without_one_is_invalid<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("h", Cell::attribute(TypeId::BYTES32, vec![0u8; 32]))],
    );
    let committed = store.commit(branch).unwrap();

    let filter = Filter::Predicate(Predicate {
        cell: "h".to_string(),
        test: Test::Compare {
            op: CompareOp::Prefix,
            type_id: TypeId::BYTES32,
            value: vec![0u8; 4],
        },
    });

    assert_eq!(
        store.count(Some(committed), &filter, None).unwrap_err(),
        StoreError::InvalidQuery,
        "only str has a prefix match"
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

/// The block lifecycle in frames — the only sequence Arkiv actually runs.
///
/// The load-bearing rule is the **two rollback scopes**: a reverted
/// transaction loses its entity ops but keeps its fee accounting. With frames
/// that is a checkpoint between the two, and a rollback that reaches only the
/// ops.
fn block_lifecycle_in_frames<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let block = store.begin(None).unwrap();

    // Transaction one succeeds: fee and ops both land.
    create_record(
        &mut store,
        block,
        record_key(1),
        &[("fee", u64_attribute(21))],
    );
    store.checkpoint(block).unwrap();
    create_record(
        &mut store,
        block,
        record_key(10),
        &[("entity", u64_attribute(1))],
    );
    store.checkpoint(block).unwrap();

    // Transaction two reverts: the fee is checkpointed before the ops run, so
    // rolling the ops back leaves it standing.
    create_record(
        &mut store,
        block,
        record_key(2),
        &[("fee", u64_attribute(21))],
    );
    store.checkpoint(block).unwrap();
    create_record(
        &mut store,
        block,
        record_key(20),
        &[("entity", u64_attribute(2))],
    );
    store.rollback(block).unwrap();

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
        "but its entity writes were rolled back"
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

/// An `And` intersects its children: a record must satisfy all of them, not
/// merely one.
fn query_and_intersects_its_children<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);

    let both = Filter::And(vec![
        equals_filter("kind", TypeId::STR, b"a".to_vec()),
        equals_filter("n", TypeId::U64, 2u64.to_be_bytes().to_vec()),
    ]);

    assert_eq!(
        matched_keys(&store, at, &paged_query(both, 0, 10)),
        vec![record_key(2)],
        "only the record satisfying both predicates"
    );
}

/// A presence test matches the records carrying the attribute, with no
/// comparison: under any type when it names none, under exactly one when it
/// does. The query language's `exists(name)` and `typeof(name) = tag`.
fn query_presence_matches_records_carrying_the_attribute<S: Store>(
    store_generator: &dyn Fn() -> S,
) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    // One record carries `tag` as a str, one as a u64, one not at all.
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("tag", str_attribute("x"))],
    );
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("tag", u64_attribute(7))],
    );
    create_record(
        &mut store,
        branch,
        record_key(3),
        &[("other", str_attribute("x"))],
    );
    let at = store.commit(branch).unwrap();

    let presence = |type_id| {
        Filter::Predicate(Predicate {
            cell: "tag".to_string(),
            test: Test::Exists { type_id },
        })
    };
    let keys = |filter| matched_keys(&store, at, &paged_query(filter, 0, 10));

    assert_eq!(
        keys(presence(None)),
        vec![record_key(1), record_key(2)],
        "any type: both carriers, and not the record without the attribute"
    );
    assert_eq!(
        keys(presence(Some(TypeId::STR))),
        vec![record_key(1)],
        "one type: only the carrier of that type"
    );
    assert_eq!(
        keys(presence(Some(TypeId::U64))),
        vec![record_key(2)],
        "the same attribute under another type is a different test"
    );
    assert_eq!(
        keys(presence(Some(TypeId::BOOL))),
        Vec::<RecordKey>::new(),
        "a type nothing carries it under matches nothing, rather than failing"
    );
    assert_eq!(
        keys(Filter::Not(Box::new(presence(None)))),
        vec![record_key(3)],
        "negated: the records not carrying it at all"
    );

    // A type that is never indexed cannot be asked for, as a prefix on a type
    // without one cannot: invalid, not empty.
    assert_eq!(
        store
            .query(
                Some(at),
                &paged_query(presence(Some(TypeId::BYTES)), 0, 10),
                None
            )
            .unwrap_err(),
        StoreError::InvalidQuery,
        "a presence test over a field-only type is invalid"
    );
}

/// A `Not` matches the records its child does not.
fn query_not_excludes_matches<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let at = committed_fixture(&mut store);

    let not_a = Filter::Not(Box::new(equals_filter("kind", TypeId::STR, b"a".to_vec())));

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
// commit atomicity
// ---------------------------------------------------------------------------
//
// These cover **logical** atomicity: a commit that fails applies nothing, and a
// commit that succeeds leaves state, digest and changeset agreeing with each
// other. They cannot cover **crash** atomicity — a process killed midway
// through `commit` — because the trait gives a caller no way to inject that
// fault. An implementation that writes its records, its digest and its head
// non-transactionally will pass everything here and still tear on a crash; that
// has to be established by reading its implementation, not by running this.

/// A commit that is refused changes nothing observable: head stays put, the
/// branch's records do not appear, and the digest does not move.
///
/// The error alone is not enough to assert. A store that advanced head and
/// *then* noticed the conflict would return the same error and have already
/// corrupted the chain.
fn failed_commit_changes_nothing<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();

    let first = store.begin(None).unwrap();
    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    let stale = store.begin(None).unwrap();
    let committed = store.commit(first).unwrap();

    let digest_before = committed_digest(&mut store);

    create_record(&mut store, stale, record_key(2), &[("n", u64_attribute(2))]);
    assert_eq!(store.commit(stale).unwrap_err(), StoreError::Conflict);

    assert_eq!(store.head(), committed, "head did not move");
    assert!(
        read_record(&store, ReadTarget::Commit(committed), record_key(2)).is_none(),
        "the refused branch's records did not land"
    );
    assert_eq!(
        committed_digest(&mut store),
        digest_before,
        "and the committed digest is unchanged"
    );
}

/// A refused commit does **not** consume the branch.
///
/// `commit` consumes on success, so a caller that cannot distinguish the two
/// has no way to clean up after a conflict — the branch would be unreachable
/// and its staged writes stranded. Discarding it must still work.
fn failed_commit_leaves_the_branch_usable<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();

    let first = store.begin(None).unwrap();
    let stale = store.begin(None).unwrap();
    store.commit(first).unwrap();

    assert_eq!(store.commit(stale).unwrap_err(), StoreError::Conflict);
    assert!(
        store.branch_info(stale).is_ok(),
        "a refused commit leaves the handle valid"
    );
    store
        .discard(stale)
        .expect("so the caller can still clean it up");
}

/// Every record of a committed branch lands, and the commit's digest is exactly
/// the one the branch reported before committing.
///
/// A commit that applied some records and not others would satisfy neither: the
/// missing records would be absent, and the digest would not match.
fn commit_applies_every_record_or_none<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();

    let ordinals = 1u8..=5;
    for ordinal in ordinals.clone() {
        create_record(
            &mut store,
            branch,
            record_key(ordinal),
            &[("n", u64_attribute(u64::from(ordinal)))],
        );
    }
    let expected_digest = store.branch_hash(branch).unwrap();
    let committed = store.commit(branch).unwrap();

    for ordinal in ordinals {
        let found = read_record(&store, ReadTarget::Commit(committed), record_key(ordinal))
            .unwrap_or_else(|| panic!("record {ordinal} landed"));
        assert_eq!(found.cell("n"), Some(&u64_attribute(u64::from(ordinal))));
    }
    assert_eq!(
        committed_digest(&mut store),
        expected_digest,
        "the commit's digest is the one its branch reported"
    );
}

/// A commit's changeset accounts for the **whole** difference between it and
/// its parent: every entry's `before` matches the parent state and its `after`
/// matches the new state, and nothing changed that the changeset omits.
///
/// This is the strongest atomicity check available through the trait. A commit
/// that applied records without logging them, or logged records it did not
/// apply, fails here — and those are exactly the shapes a torn commit takes.
fn changes_account_for_the_whole_difference<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();

    // Parent commit: two records, one of which the next commit will touch.
    let parent_branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        parent_branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    create_record(
        &mut store,
        parent_branch,
        record_key(2),
        &[("n", u64_attribute(2))],
    );
    let parent = store.commit(parent_branch).unwrap();

    // A commit that creates one record, mutates one, deletes one, and leaves
    // one alone — all four shapes in a single changeset.
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(3),
        &[("n", u64_attribute(3))],
    );
    store
        .patch(
            branch,
            record_key(1),
            None,
            set_cell("n", u64_attribute(99)),
            None,
        )
        .unwrap();
    store.delete(branch, record_key(2), None, None).unwrap();
    let committed = store.commit(branch).unwrap();

    let changes = store.changes(committed).unwrap();

    // Each entry's before/after must match what the two commits actually hold.
    for change in &changes {
        assert_eq!(
            change.before,
            read_record(&store, ReadTarget::Commit(parent), change.key),
            "changeset `before` disagrees with the parent commit for {:?}",
            change.key
        );
        assert_eq!(
            change.after,
            read_record(&store, ReadTarget::Commit(committed), change.key),
            "changeset `after` disagrees with the new commit for {:?}",
            change.key
        );
    }

    // And nothing moved that the changeset failed to mention. Record 4 is
    // absent from both, record 1..=3 cover the three shapes above.
    for ordinal in 1u8..=4 {
        let key = record_key(ordinal);
        let before = read_record(&store, ReadTarget::Commit(parent), key);
        let after = read_record(&store, ReadTarget::Commit(committed), key);
        let logged = changes.iter().any(|change| change.key == key);
        assert_eq!(
            before != after,
            logged,
            "record {ordinal} changed without being logged, or was logged without changing"
        );
    }
}

/// A refused commit writes no changeset, so the commit id it would have taken
/// stays unused rather than holding an orphaned log.
fn failed_commit_writes_no_changeset<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();

    let first = store.begin(None).unwrap();
    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    let stale = store.begin(None).unwrap();
    let committed = store.commit(first).unwrap();

    create_record(&mut store, stale, record_key(2), &[("n", u64_attribute(2))]);
    assert_eq!(store.commit(stale).unwrap_err(), StoreError::Conflict);

    // The id the refused commit would have claimed must not resolve at all.
    assert!(
        store.changes(CommitId(committed.0 + 1)).is_err(),
        "a refused commit leaves no changeset behind"
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

/// The tag reads back from the commit, not just the commit from the tag.
///
/// reth needs both directions: `commit_by_tag` answers `state_by_block_hash`,
/// and this answers `block_hash(number)`, which every provider must implement.
fn a_commits_tag_is_readable_from_the_commit<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let tag = [0xCD; 32];
    let committed = store.commit_tagged(branch, tag).unwrap();

    assert_eq!(store.tag_of(committed).unwrap(), Some(tag));
    // And the two directions agree.
    assert_eq!(store.commit_by_tag(tag).unwrap(), Some(committed));
}

/// A commit made without a tag has none — absent, not a zero tag. Reporting
/// `[0u8; 32]` would be a block hash that no block has.
fn an_untagged_commit_has_no_tag<S: StoreExt>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );

    let committed = store.commit(branch).unwrap();
    assert_eq!(store.tag_of(committed).unwrap(), None);
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

    let at_commit_time = store.branch_hash(branch).unwrap();
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
    let batched = store_generator();
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
        batched.branch_hash(batched_branch).unwrap(),
        serial.branch_hash(serial_branch).unwrap()
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

// ---------------------------------------------------------------------------
// Segments: the append-only tier
// ---------------------------------------------------------------------------

/// A single-column row carrying `bytes`.
fn row(bytes: &[u8]) -> SegmentRow {
    vec![bytes.to_vec()]
}

/// A distinct segment key per `ordinal`.
fn segment_key(ordinal: u8) -> SegmentKey {
    let mut key = [0u8; 32];
    key[31] = ordinal;
    SegmentKey(key)
}

/// A branch sealed and ready to take appends.
fn sealed_branch<S: Store>(store: &S) -> BranchId {
    let branch = store.begin(None).unwrap();
    store.seal(branch).unwrap();
    branch
}

/// Appends are legal only after `seal`. This is what keeps the tier outside
/// the state root: by the time a row can be written, the roots are fixed.
fn appending_before_seal_is_refused<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = store.begin(None).unwrap();

    assert_eq!(
        store.segment_append(branch, "blocks", None, row(b"header")),
        Err(StoreError::NotSealed),
    );
}

/// A staged row is not readable until the branch commits.
fn an_appended_row_is_invisible_until_commit<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    let key = segment_key(1);
    store
        .segment_append(branch, "blocks", Some(key), row(b"header"))
        .unwrap();

    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Ordinal(0)),
        Ok(None),
        "an uncommitted row is not addressable by ordinal",
    );
    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Key(key)),
        Ok(None),
        "nor by key -- the row and its name become visible together, at commit",
    );
}

/// After commit, the ordinal returned by `append` addresses the row.
fn a_committed_row_reads_back_by_ordinal<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    let ordinal = store
        .segment_append(branch, "blocks", None, row(b"header"))
        .unwrap();
    store.commit(branch).unwrap();

    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Ordinal(ordinal)),
        Ok(Some(row(b"header"))),
    );
}

/// After commit, a row's key addresses it. This is the property that lets
/// Arkiv keep no index: a transaction hash *is* its row's name.
fn a_committed_row_reads_back_by_key<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    let key = segment_key(7);
    store
        .segment_append(branch, "transactions", Some(key), row(b"tx"))
        .unwrap();
    store.commit(branch).unwrap();

    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Key(key)),
        Ok(Some(row(b"tx"))),
    );
    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Key(segment_key(8))),
        Ok(None),
        "an unbound key is absent, not an error",
    );
}

/// Ordinals start at zero, increase by one, and follow append order.
fn ordinals_are_dense_and_in_append_order<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    let first = store
        .segment_append(branch, "blocks", None, row(b"one"))
        .unwrap();
    let second = store
        .segment_append(branch, "blocks", None, row(b"two"))
        .unwrap();
    let third = store
        .segment_append(branch, "blocks", None, row(b"three"))
        .unwrap();
    store.commit(branch).unwrap();

    assert_eq!((first, second, third), (0, 1, 2));
    for (ordinal, expected) in [(0, b"one".as_slice()), (1, b"two"), (2, b"three")] {
        assert_eq!(
            store.segment_get("blocks", SegmentAddress::Ordinal(ordinal)),
            Ok(Some(row(expected))),
            "ordinal {ordinal} should hold the row appended in that position",
        );
    }
}

/// A duplicate key is refused, and the row already bound to it is untouched.
fn a_duplicate_key_is_refused_and_overwrites_nothing<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let key = segment_key(1);

    let first = sealed_branch(&store);
    store
        .segment_append(first, "transactions", Some(key), row(b"original"))
        .unwrap();
    store.commit(first).unwrap();

    let second = sealed_branch(&store);
    assert_eq!(
        store.segment_append(second, "transactions", Some(key), row(b"replacement")),
        Err(StoreError::AlreadyExists),
    );
    // Also within one branch, before anything has committed.
    let third = sealed_branch(&store);
    store
        .segment_append(third, "transactions", Some(segment_key(2)), row(b"a"))
        .unwrap();
    assert_eq!(
        store.segment_append(third, "transactions", Some(segment_key(2)), row(b"b")),
        Err(StoreError::AlreadyExists),
    );

    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Key(key)),
        Ok(Some(row(b"original"))),
        "the bound row survived the refused append",
    );
}

/// Rows staged on a branch that never commits are gone with it, and the key
/// they claimed is free again.
fn a_discarded_branchs_rows_never_appear<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let key = segment_key(1);

    let abandoned = sealed_branch(&store);
    store
        .segment_append(abandoned, "blocks", Some(key), row(b"orphan"))
        .unwrap();
    store.discard(abandoned).unwrap();

    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Ordinal(0)),
        Ok(None),
    );

    let kept = sealed_branch(&store);
    store
        .segment_append(kept, "blocks", Some(key), row(b"real"))
        .unwrap();
    store.commit(kept).unwrap();

    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Key(key)),
        Ok(Some(row(b"real"))),
        "the discarded branch left its key unbound",
    );
}

/// `range_of` reports the ordinals one commit appended, which is what groups
/// rows by the block that wrote them.
fn range_of_reports_what_one_commit_appended<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();

    let first = sealed_branch(&store);
    store
        .segment_append(first, "receipts", None, row(b"r0"))
        .unwrap();
    store
        .segment_append(first, "receipts", None, row(b"r1"))
        .unwrap();
    let one = store.commit(first).unwrap();

    let second = sealed_branch(&store);
    store
        .segment_append(second, "receipts", None, row(b"r2"))
        .unwrap();
    let two = store.commit(second).unwrap();

    assert_eq!(store.segment_range_of("receipts", one), Ok(0..2));
    assert_eq!(store.segment_range_of("receipts", two), Ok(2..3));
}

/// `rows_of` is the rows of `range_of`, in append order.
fn rows_of_returns_that_commits_rows_in_order<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();

    let first = sealed_branch(&store);
    store
        .segment_append(first, "receipts", None, row(b"skipped"))
        .unwrap();
    store.commit(first).unwrap();

    let second = sealed_branch(&store);
    store
        .segment_append(second, "receipts", None, row(b"wanted-0"))
        .unwrap();
    store
        .segment_append(second, "receipts", None, row(b"wanted-1"))
        .unwrap();
    let wanted = store.commit(second).unwrap();

    assert_eq!(
        store.segment_rows_of("receipts", wanted),
        Ok(vec![row(b"wanted-0"), row(b"wanted-1")]),
        "only that commit's rows, and in the order they were appended",
    );
}

/// A commit that appended nothing, and a segment nobody has written, are
/// empty rather than errors.
fn an_untouched_segment_is_empty_not_an_error<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = store.begin(None).unwrap();
    let quiet = store.commit(branch).unwrap();

    assert_eq!(store.segment_range_of("blocks", quiet), Ok(0..0));
    assert_eq!(store.segment_rows_of("blocks", quiet), Ok(vec![]));
    assert_eq!(
        store.segment_get("never-written", SegmentAddress::Ordinal(0)),
        Ok(None),
    );
}

/// Segments do not share an ordinal space or a key space.
fn segments_are_independent_of_each_other<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    let key = segment_key(1);

    let in_blocks = store
        .segment_append(branch, "blocks", Some(key), row(b"a block"))
        .unwrap();
    // The same key in another segment is not a duplicate.
    let in_receipts = store
        .segment_append(branch, "receipts", Some(key), row(b"a receipt"))
        .unwrap();
    store.commit(branch).unwrap();

    assert_eq!(
        (in_blocks, in_receipts),
        (0, 0),
        "each segment counts from zero",
    );
    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Key(key)),
        Ok(Some(row(b"a block"))),
    );
    assert_eq!(
        store.segment_get("receipts", SegmentAddress::Key(key)),
        Ok(Some(row(b"a receipt"))),
    );
}

/// A key is optional: a row with no name is still appended and addressable by
/// ordinal. Receipts need this -- they are reached through their block.
fn a_row_needs_no_key<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    store
        .segment_append(branch, "receipts", None, row(b"unnamed"))
        .unwrap();
    store
        .segment_append(branch, "receipts", None, row(b"also unnamed"))
        .unwrap();
    let committed = store.commit(branch).unwrap();

    assert_eq!(store.segment_range_of("receipts", committed), Ok(0..2));
    assert_eq!(
        store.segment_get("receipts", SegmentAddress::Ordinal(1)),
        Ok(Some(row(b"also unnamed"))),
    );
}

/// Appending a batch is indistinguishable from appending its rows one by one:
/// same ordinals, same order, same names bound.
fn a_batch_append_matches_one_at_a_time<S: Store>(store_generator: &dyn Fn() -> S) {
    let batched = store_generator();
    let branch = sealed_branch(&batched);
    let ordinals = batched
        .segment_append_many(
            branch,
            &[
                ("blocks", Some(segment_key(1)), row(b"one")),
                ("receipts", None, row(b"r")),
                ("blocks", Some(segment_key(2)), row(b"two")),
            ],
        )
        .unwrap();
    let batched_commit = batched.commit(branch).unwrap();

    // The same three rows, appended individually against a fresh store.
    let singly = store_generator();
    let branch = sealed_branch(&singly);
    let mut expected = vec![];
    for (segment, key, body) in [
        ("blocks", Some(segment_key(1)), row(b"one")),
        ("receipts", None, row(b"r")),
        ("blocks", Some(segment_key(2)), row(b"two")),
    ] {
        expected.push(singly.segment_append(branch, segment, key, body).unwrap());
    }
    let singly_commit = singly.commit(branch).unwrap();

    assert_eq!(ordinals, expected, "the ordinals a batch hands back");
    assert_eq!(
        ordinals,
        vec![0, 0, 1],
        "each segment counts separately, in append order",
    );
    for segment in ["blocks", "receipts"] {
        assert_eq!(
            batched.segment_rows_of(segment, batched_commit),
            singly.segment_rows_of(segment, singly_commit),
            "{segment} holds the same rows either way",
        );
    }
    assert_eq!(
        batched.segment_get("blocks", SegmentAddress::Key(segment_key(2))),
        Ok(Some(row(b"two"))),
        "and the names are bound",
    );
}

/// A batch carrying a name twice is refused, and discarding the branch is what
/// removes whatever it had staged.
///
/// The batch is not atomic on purpose — see `segment_append_many`. What must
/// hold is that a refused batch cannot reach a reader, and discarding is the
/// remedy a caller already has for a block it cannot complete.
fn a_batch_with_a_duplicate_name_is_refused<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);

    let refused = store.segment_append_many(
        branch,
        &[
            ("blocks", Some(segment_key(1)), row(b"first")),
            ("blocks", Some(segment_key(1)), row(b"again")),
        ],
    );
    assert_eq!(refused, Err(StoreError::AlreadyExists));

    store.discard(branch).unwrap();
    let quiet = store.begin(None).unwrap();
    let committed = store.commit(quiet).unwrap();
    assert_eq!(
        store.segment_rows_of("blocks", committed),
        Ok(vec![]),
        "the discarded branch took the refused batch with it",
    );
    assert_eq!(
        store.segment_get("blocks", SegmentAddress::Key(segment_key(1))),
        Ok(None),
        "and the name it claimed is free again",
    );
}

/// A name resolves to the ordinal its row was appended at, and the two
/// addresses then reach the same row.
///
/// This is what lets a host keep a dense numbering of its own over named rows:
/// reth identifies a transaction by a `TxNumber`, which is the ordinal, while
/// the chain identifies it by hash, which is the name. If the two disagreed,
/// a transaction looked up by number would be a different transaction.
fn a_name_resolves_to_the_ordinal_it_was_appended_at<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();
    let branch = sealed_branch(&store);
    let first = segment_key(1);
    let second = segment_key(2);
    let first_ordinal = store
        .segment_append(branch, "transactions", Some(first), row(b"one"))
        .unwrap();
    // An unnamed row in between, so the ordinals cannot be read off the names.
    store
        .segment_append(branch, "transactions", None, row(b"unnamed"))
        .unwrap();
    let second_ordinal = store
        .segment_append(branch, "transactions", Some(second), row(b"two"))
        .unwrap();
    store.commit(branch).unwrap();

    assert_eq!(
        store.segment_ordinal_of("transactions", first),
        Ok(Some(first_ordinal)),
    );
    assert_eq!(
        store.segment_ordinal_of("transactions", second),
        Ok(Some(second_ordinal)),
    );
    assert_eq!(
        store.segment_ordinal_of("transactions", segment_key(9)),
        Ok(None),
        "an unbound name is absent, not an error",
    );
    // The point of the two addresses: both reach the row.
    assert_eq!(
        store.segment_get("transactions", SegmentAddress::Ordinal(second_ordinal)),
        store.segment_get("transactions", SegmentAddress::Key(second)),
    );
}

/// A commit that appended nothing to an existing segment reports an empty
/// range *at that segment's end*, not `0..0`.
///
/// The position is as load-bearing as the count. A host asking "where do this
/// block's transactions start?" gets `first_tx_num` from the range's start,
/// and a block with no transactions has to be distinguishable from one whose
/// transactions genuinely start at ordinal zero. On a quiet chain the empty
/// block is the common case, so `0..0` would be wrong far more often than
/// right.
fn an_empty_range_still_says_where_the_commit_stood<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();

    // One commit puts two rows in `transactions`.
    let branch = sealed_branch(&store);
    store
        .segment_append(branch, "transactions", None, row(b"one"))
        .unwrap();
    store
        .segment_append(branch, "transactions", None, row(b"two"))
        .unwrap();
    let first = store.commit(branch).unwrap();
    assert_eq!(store.segment_range_of("transactions", first), Ok(0..2));

    // The next touches only `blocks`, leaving `transactions` alone.
    let branch = sealed_branch(&store);
    store
        .segment_append(branch, "blocks", None, row(b"header"))
        .unwrap();
    let second = store.commit(branch).unwrap();

    assert_eq!(
        store.segment_range_of("transactions", second),
        Ok(2..2),
        "the empty range sits at the end the segment had reached, not at zero",
    );
    assert_eq!(
        store.segment_range_of("blocks", second),
        Ok(0..1),
        "and the segment it did append to is unaffected",
    );
    assert_eq!(
        store.segment_range_of("receipts", second),
        Ok(0..0),
        "a segment the store has never seen has no length to report",
    );
}
