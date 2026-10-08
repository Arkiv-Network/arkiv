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
    patch_may_remove_the_last_cell(store_generator);
    create_with_no_cells_makes_an_empty_record(store_generator);
    delete_removes_record(store_generator);
    reserved_cell_name_rejected(store_generator);
    field_only_type_cannot_be_attribute(store_generator); // I do not understand this
    invalid_type_id_rejected(store_generator);
    sealed_roots_track_content(store_generator);
    query_sees_committed_state(store_generator);
    query_dnf_unions_and_dedups(store_generator);
    query_defaults_to_creation_order(store_generator);
    genesis_lands_as_the_first_commit(store_generator);
    genesis_refuses_to_apply_twice(store_generator);
    genesis_is_readable_at_its_commit(store_generator);
    genesis_is_all_or_nothing(store_generator);
    block_lifecycle_in_frames(store_generator);
    a_commit_invalidates_the_other_branches(store_generator);
    projection_limits_returned_cells(store_generator);
    query_and_group_intersects_predicates(store_generator);
    query_negated_predicate_excludes_matches(store_generator);
    query_pages_with_offset_and_limit(store_generator);
    query_reports_total_matched_beyond_the_page(store_generator);
    query_sorts_ascending_and_descending(store_generator);
    query_sort_ranks_missing_cells_lowest_and_ties_oldest_first(store_generator);
    failed_commit_changes_nothing(store_generator);
    a_refused_commit_consumes_the_branch(store_generator);
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
/// before `record_key(2)` — which lets the result-order assertions tell key
/// order from creation order.
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

/// The roots an empty branch over head seals to, using only [`Store`].
///
/// The branch stages nothing, so its roots are a function of committed state
/// alone — which is how the atomicity assertions observe committed state
/// without needing [`StoreExt::commit_hash`]. They are not the head commit's
/// own roots: a seal may add bookkeeping of its own (GolemDB records the
/// previous commit's roots), and the probe is never committed.
fn committed_roots<S: Store>(store: &S) -> ([u8; 32], [u8; 32]) {
    let probe = store.begin(None).expect("open a probe branch");
    let sealed = store.seal(probe).expect("seal it");
    store.discard(probe).expect("and drop it again");
    (sealed.state_root, sealed.index_root)
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

/// A created record reads back with its key and its cells.
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
    assert_eq!(store.head().0, head.0 + 1);
    assert_eq!(
        store.discard(b).unwrap_err(),
        StoreError::HandleInvalid,
        "the commit invalidated the other, so there is nothing left to discard"
    );
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

/// A branch whose origin is no longer head cannot commit. This is the no-fork
/// guarantee: a second block at one height is refused, and a host should
/// treat that as fatal rather than retry it.
fn commit_guard_rejects_second_branch_committal<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let first = store.begin(None).unwrap();
    let stale = store.begin(None).unwrap();

    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    create_record(&mut store, stale, record_key(2), &[("n", u64_attribute(2))]);
    store.commit(first).unwrap();

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

/// Patching away a record's last cell leaves the record empty, not absent —
/// removing the record is `delete`'s job.
fn patch_may_remove_the_last_cell<S: Store>(store_generator: &dyn Fn() -> S) {
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
            vec![("n".to_string(), CellChange::Remove)],
            None,
        )
        .expect("the last cell may be removed");

    let found = read_record(&store, ReadTarget::Branch(branch), record_key(1))
        .expect("the record is still present");
    assert!(found.cells.is_empty(), "with no cells left");
}

/// A record may be created with no cells at all. It exists from `create` until
/// `delete`, whatever cells it holds, so an empty one reads back present.
fn create_with_no_cells_makes_an_empty_record<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    create_record(&mut store, branch, record_key(1), &[]);

    let found = read_record(&store, ReadTarget::Branch(branch), record_key(1))
        .expect("an empty record is present");
    assert_eq!(found.key, record_key(1));
    assert!(found.cells.is_empty(), "with no cells");
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
                cell_map(&[("#key", u64_attribute(1))]),
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

/// Seal a fresh branch over head holding one record with `cells`, or nothing
/// at all for `None`, and drop it again.
fn seal_one_record<S: Store>(store: &mut S, cells: Option<&[(&str, Cell)]>) -> SealedCommit {
    let branch = store.begin(None).unwrap();
    if let Some(cells) = cells {
        create_record(store, branch, record_key(1), cells);
    }
    let sealed = store.seal(branch).unwrap();
    store.discard(branch).unwrap();
    sealed
}

/// The roots a branch seals to follow its content: the same writes over one
/// head seal to the same roots, and different content to different ones.
///
/// Each content state gets its own branch, because a sealed branch takes no
/// more writes; all are over the same head, so only their writes differ.
///
/// Roots are **not** a function of content alone, so this does not assert
/// that undoing a write restores them. In GolemDB a record that was created
/// and then deleted still leaves the roots different from never creating it.
fn sealed_roots_track_content<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let empty = seal_one_record(&mut store, None);
    let one = seal_one_record(&mut store, Some(&[("n", u64_attribute(1))]));

    assert_eq!(
        seal_one_record(&mut store, Some(&[("n", u64_attribute(1))])),
        one,
        "the same writes over the same head seal to the same roots"
    );
    assert_ne!(
        one.state_root, empty.state_root,
        "a new record moves the state root"
    );
    assert_ne!(
        seal_one_record(&mut store, Some(&[("n", u64_attribute(2))])).state_root,
        one.state_root,
        "and so does a different value"
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

/// With no sort, results come back in the order the records were created in,
/// not in key order. A record deleted and created again counts from its new
/// creation, so it comes back last.
fn query_defaults_to_creation_order<S: Store>(store_generator: &dyn Fn() -> S) {
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
    let first = store.commit(branch).unwrap();

    let query = paged_query(equals_filter("kind", TypeId::STR, b"a".to_vec()), 0, 10);
    assert_eq!(
        matched_keys(&store, first, &query),
        vec![record_key(3), record_key(1), record_key(2)],
        "creation order, not key order"
    );

    let branch = store.begin(None).unwrap();
    store.delete(branch, record_key(3), None, None).unwrap();
    create_record(
        &mut store,
        branch,
        record_key(3),
        &[("kind", str_attribute("a"))],
    );
    let second = store.commit(branch).unwrap();

    assert_eq!(
        matched_keys(&store, second, &query),
        vec![record_key(1), record_key(2), record_key(3)],
        "a record created again is the newest"
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
            (record_key(2), cell_map(&[("#key", u64_attribute(1))])),
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

/// A commit invalidates every other open branch: they were opened over the old
/// head, and are not carried over to the new one.
///
/// The first call to touch such a branch is refused and releases it —
/// [`StoreError::Conflict`] if that call is `commit` (see
/// [`commit_guard_rejects_second_branch_committal`]), and
/// [`StoreError::HandleInvalid`] otherwise. Any later call is `HandleInvalid`.
fn a_commit_invalidates_the_other_branches<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();
    let held = store.begin(None).unwrap();
    create_record(&mut store, held, record_key(1), &[("n", u64_attribute(1))]);

    let other = store.begin(None).unwrap();
    store.commit(other).unwrap();

    assert_eq!(
        store
            .get(ReadTarget::Branch(held), record_key(1), None, None)
            .unwrap_err(),
        StoreError::HandleInvalid,
        "a branch over the old head no longer reads"
    );
    assert_eq!(
        store.commit(held).unwrap_err(),
        StoreError::HandleInvalid,
        "and that refusal released it, so even `commit` is no longer a conflict"
    );
}

/// A projection returns only the named cells. The record's key is always
/// present — it identifies the record, it is not content.
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

/// A record lacking the sort cell ranks below every value, so it comes first
/// ascending and last descending. Ties break on creation order, oldest first,
/// in both directions — so the ordering is total and two implementations
/// agreeing on the match set also agree on the page.
fn query_sort_ranks_missing_cells_lowest_and_ties_oldest_first<S: Store>(
    store_generator: &dyn Fn() -> S,
) {
    let mut store = store_generator();
    let branch = store.begin(None).unwrap();
    // Two records share a sort value, created against key order so the
    // tie-break shows; and one has no sort cell at all.
    create_record(
        &mut store,
        branch,
        record_key(2),
        &[("n", u64_attribute(1))],
    );
    create_record(
        &mut store,
        branch,
        record_key(1),
        &[("n", u64_attribute(1))],
    );
    create_record(
        &mut store,
        branch,
        record_key(3),
        &[("other", u64_attribute(9))],
    );
    let at = store.commit(branch).unwrap();

    let sorted = |direction| Query {
        sort: Some(Sort {
            cell: "n".to_string(),
            direction,
        }),
        ..paged_query(Filter::default(), 0, 10)
    };

    assert_eq!(
        matched_keys(&store, at, &sorted(SortDirection::Ascending)),
        vec![record_key(3), record_key(2), record_key(1)],
        "ascending: the record without the cell first, then the tie oldest first"
    );
    assert_eq!(
        matched_keys(&store, at, &sorted(SortDirection::Descending)),
        vec![record_key(2), record_key(1), record_key(3)],
        "descending: the tie still oldest first, and the record without the cell last"
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
/// branch's records do not appear, and committed state seals to the same
/// roots.
///
/// The error alone is not enough to assert. A store that advanced head and
/// *then* noticed the conflict would return the same error and have already
/// corrupted the chain.
fn failed_commit_changes_nothing<S: Store>(store_generator: &dyn Fn() -> S) {
    let mut store = store_generator();

    // Two branches over one head, each with a write staged; the first wins.
    let first = store.begin(None).unwrap();
    create_record(&mut store, first, record_key(1), &[("n", u64_attribute(1))]);
    let stale = store.begin(None).unwrap();
    create_record(&mut store, stale, record_key(2), &[("n", u64_attribute(2))]);
    let committed = store.commit(first).unwrap();

    let roots_before = committed_roots(&store);
    assert_eq!(store.commit(stale).unwrap_err(), StoreError::Conflict);

    assert_eq!(store.head(), committed, "head did not move");
    assert!(
        read_record(&store, ReadTarget::Commit(committed), record_key(2)).is_none(),
        "the refused branch's records did not land"
    );
    assert_eq!(
        committed_roots(&store),
        roots_before,
        "and committed state seals to the same roots"
    );
}

/// A refused commit consumes the branch, as a successful one does, so the
/// caller has nothing to clean up: any later call on it is
/// [`StoreError::HandleInvalid`].
fn a_refused_commit_consumes_the_branch<S: Store>(store_generator: &dyn Fn() -> S) {
    let store = store_generator();

    let first = store.begin(None).unwrap();
    let stale = store.begin(None).unwrap();
    store.commit(first).unwrap();

    assert_eq!(store.commit(stale).unwrap_err(), StoreError::Conflict);
    assert_eq!(
        store.commit(stale).unwrap_err(),
        StoreError::HandleInvalid,
        "the refused commit consumed the handle, so a retry is not even a conflict"
    );
    assert_eq!(
        store.discard(stale).unwrap_err(),
        StoreError::HandleInvalid,
        "so there is nothing left to discard"
    );
}

/// Every record of a committed branch lands.
///
/// That the commit also keeps the roots its branch sealed to is not asserted
/// here: [`Store`] has no way to read a commit's roots back. It is
/// [`digest_same_on_branch_committal`]'s, through [`StoreExt::commit_hash`].
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
    let committed = store.commit(branch).unwrap();

    for ordinal in ordinals {
        let found = read_record(&store, ReadTarget::Commit(committed), record_key(ordinal))
            .unwrap_or_else(|| panic!("record {ordinal} landed"));
        assert_eq!(found.cell("n"), Some(&u64_attribute(u64::from(ordinal))));
    }
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
    create_record(&mut store, stale, record_key(2), &[("n", u64_attribute(2))]);
    let committed = store.commit(first).unwrap();

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
