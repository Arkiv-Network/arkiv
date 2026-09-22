//! [`MemStore`] — the in-memory reference implementation of
//! [`Store`](arkiv_interfaces::store::Store).
//!
//! Written to be **obviously correct**, not fast. It is the first thing the
//! conformance suite is proved against, and the oracle any other
//! implementation — a mock over another engine, or the real store — is
//! compared to. Where the spec allows latitude, this picks the dullest option
//! and says so.
//!
//! Two deliberate simplifications, both safe because nothing here is
//! consensus:
//!
//! - **`fork` copies the parent's state** instead of layering a diff over it.
//!   The real store makes `fork` O(1); here O(n) buys a trivially correct read
//!   path with no stack walking. Observable behaviour is identical.
//! - **[`MemStore::branch_hash`] is a deterministic digest, not a
//!   commitment.** It recomputes over the canonical encoding of the whole
//!   branch and mixes with FNV-1a. That is enough to detect divergence between
//!   two implementations, which is all the conformance suite asks. It is *not*
//!   collision-resistant and proves nothing. Anything relying on a real
//!   commitment must use the real store.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use arkiv_interfaces::store::{
    AndGroup, BranchId, BranchInfo, BranchVersion, Budget, Cell, CellChange, CellKind, CellName,
    CommitId, CompareOp, CostUnits, Filter, Metered, Origin, Predicate, Query, QueryResult,
    ReadTarget, Receipt, Record, RecordChange, RecordKey, RecordVersion, ScheduleVersion, Sort,
    SortDirection, Store, StoreError, StoreExt, TypeId, WriteOp, WriteOutcome, validate_cell_name,
};

/// One record's stored form: its cells, plus the `#version` meta entry the
/// spec keeps alongside them. Presence of this struct *is* record existence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct RecordData {
    version: RecordVersion,
    cells: BTreeMap<CellName, Cell>,
}

impl RecordData {
    /// Render this record for a caller, keeping only the projected cells when
    /// a projection was asked for.
    ///
    /// Cells come out in ascending name order, because the backing map is
    /// ordered — so two equal records always compare equal.
    fn to_record(&self, key: RecordKey, projection: Option<&[CellName]>) -> Record {
        let cells = self
            .cells
            .iter()
            .filter(|(name, _)| projection.is_none_or(|wanted| wanted.contains(name)))
            .map(|(name, cell)| (name.clone(), cell.clone()))
            .collect();
        Record {
            key,
            version: self.version,
            cells,
        }
    }
}

/// The committed state as of one commit, plus what arriving here changed.
#[derive(Debug, Clone)]
struct Snapshot {
    state: BTreeMap<RecordKey, RecordData>,
    hash: [u8; 32],
    /// The opaque host identifier attached by
    /// [`commit_tagged`](StoreExt::commit_tagged), if any. For Arkiv, a block
    /// hash.
    tag: Option<[u8; 32]>,
    changes: Vec<RecordChange>,
}

/// One open branch.
#[derive(Debug, Clone)]
struct BranchState {
    origin: Origin,
    version: BranchVersion,
    depth: u32,
    /// This branch's whole view of the world: its origin's state with its own
    /// writes applied. See the crate docs on why this is a copy, not a layer.
    state: BTreeMap<RecordKey, RecordData>,
}

/// An in-memory [`Store`]. See the crate docs for what it is and is not.
#[derive(Debug)]
pub struct MemStore {
    /// Indexed by [`CommitId`], so `commits[0]` is always genesis.
    commits: Vec<Snapshot>,
    branches: BTreeMap<u64, BranchState>,
    /// Monotonic, never rewound — handles are never reused.
    next_branch: u64,
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemStore {
    /// A store at genesis: commit 0, empty state, no open branches.
    pub fn new() -> Self {
        let genesis = BTreeMap::new();
        Self {
            commits: alloc::vec![Snapshot {
                hash: content_digest(&genesis),
                state: genesis,
                tag: None,
                changes: Vec::new(),
            }],
            branches: BTreeMap::new(),
            next_branch: 0,
        }
    }

    /// Look up an open branch, or report the handle as spent.
    fn branch_state(&self, branch: BranchId) -> Result<&BranchState, StoreError> {
        self.branches
            .get(&branch.0)
            .ok_or(StoreError::HandleInvalid)
    }

    /// Mutable counterpart of [`branch_state`](Self::branch_state).
    fn branch_state_mut(&mut self, branch: BranchId) -> Result<&mut BranchState, StoreError> {
        self.branches
            .get_mut(&branch.0)
            .ok_or(StoreError::HandleInvalid)
    }

    /// Look up a commit, or report it as outside the retention window.
    fn commit_snapshot(&self, commit: CommitId) -> Result<&Snapshot, StoreError> {
        self.commits
            .get(commit.0 as usize)
            .ok_or(StoreError::NotFound)
    }

    /// The state a read resolves against — a branch's own view, or a commit's.
    fn state_for_target(
        &self,
        target: ReadTarget,
    ) -> Result<&BTreeMap<RecordKey, RecordData>, StoreError> {
        match target {
            ReadTarget::Branch(branch) => Ok(&self.branch_state(branch)?.state),
            ReadTarget::Commit(commit) => Ok(&self.commit_snapshot(commit)?.state),
        }
    }

    /// Open a branch for writing: validate the handle and count the batch.
    ///
    /// Every write call goes through here, which is what keeps "+1 per call"
    /// true in exactly one place.
    fn begin_write(&mut self, branch: BranchId) -> Result<&mut BranchState, StoreError> {
        let state = self.branch_state_mut(branch)?;
        state.version = BranchVersion(state.version.0 + 1);
        Ok(state)
    }

    /// Reinstate a branch wholesale — the rollback half of an all-or-nothing
    /// [`apply`](StoreExt::apply).
    fn restore_branch(&mut self, branch: BranchId, saved: BranchState) {
        self.branches.insert(branch.0, saved);
    }
}

/// Nothing here meters, so every receipt is the same zero.
///
/// Cost is the real store's concern, priced against a schedule this
/// implementation does not have. Inventing numbers would give call sites
/// something that looks like a cost and means nothing.
const fn free_receipt() -> Receipt {
    Receipt {
        cost: CostUnits(0),
        schedule: ScheduleVersion(0),
    }
}

/// Wrap a value in the zero receipt. See [`free_receipt`].
fn with_free_receipt<T>(value: T) -> Metered<T> {
    Metered::new(value, free_receipt())
}

impl Store for MemStore {
    fn head(&self) -> CommitId {
        CommitId(self.commits.len() as u64 - 1)
    }

    fn begin(&mut self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        let origin = at.unwrap_or_else(|| self.head());
        let state = self.commit_snapshot(origin)?.state.clone();

        let branch = BranchId(self.next_branch);
        self.next_branch += 1;
        self.branches.insert(
            branch.0,
            BranchState {
                origin: Origin::Commit(origin),
                version: BranchVersion(0),
                depth: 0,
                state,
            },
        );
        Ok(branch)
    }

    fn fork(&mut self, parent: BranchId) -> Result<BranchId, StoreError> {
        let parent_state = self.branch_state(parent)?;
        let child_state = BranchState {
            // Recording the parent's version here is what the merge guard
            // later checks against.
            origin: Origin::Branch(parent, parent_state.version),
            version: BranchVersion(0),
            depth: parent_state.depth + 1,
            state: parent_state.state.clone(),
        };

        let child = BranchId(self.next_branch);
        self.next_branch += 1;
        self.branches.insert(child.0, child_state);
        Ok(child)
    }

    fn merge(&mut self, child: BranchId) -> Result<BranchVersion, StoreError> {
        let child_state = self.branch_state(child)?.clone();
        let Origin::Branch(parent, forked_at) = child_state.origin else {
            // A root branch commits; it does not merge.
            return Err(StoreError::HandleInvalid);
        };

        // The guard: the parent must not have moved since the fork. In
        // blockchain mode this never fires — it asserts the sequential
        // discipline rather than resolving contention.
        if self.branch_state(parent)?.version != forked_at {
            return Err(StoreError::Conflict);
        }

        self.branches.remove(&child.0);
        let parent_state = self.branch_state_mut(parent)?;
        // The child started as a copy of the parent and diverged only by its
        // own writes, so adopting its state wholesale *is* the fold.
        parent_state.state = child_state.state;
        parent_state.version = BranchVersion(parent_state.version.0 + 1);
        Ok(parent_state.version)
    }

    fn discard(&mut self, branch: BranchId) -> Result<(), StoreError> {
        if self.branches.remove(&branch.0).is_none() {
            return Err(StoreError::HandleInvalid);
        }

        // Descendants are grounded on a handle that no longer exists, so they
        // go too — the spec's "and its open descendants".
        let orphans: Vec<u64> = self
            .branches
            .iter()
            .filter(|(_, state)| matches!(state.origin, Origin::Branch(p, _) if p == branch))
            .map(|(id, _)| *id)
            .collect();
        for orphan in orphans {
            self.discard(BranchId(orphan))?;
        }
        Ok(())
    }

    fn commit(&mut self, root: BranchId) -> Result<CommitId, StoreError> {
        let root_state = self.branch_state(root)?.clone();
        let Origin::Commit(origin) = root_state.origin else {
            // Child branches may only merge or discard.
            return Err(StoreError::HandleInvalid);
        };
        // The no-fork guarantee: a branch whose origin has been overtaken
        // cannot seal. A host should treat this as fatal, not retry it.
        if origin != self.head() {
            return Err(StoreError::Conflict);
        }

        let previous = &self.commit_snapshot(origin)?.state;
        let changes = record_changes_between(previous, &root_state.state);
        let hash = content_digest(&root_state.state);

        self.branches.remove(&root.0);
        self.commits.push(Snapshot {
            state: root_state.state,
            hash,
            tag: None,
            changes,
        });
        Ok(self.head())
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        let state = self.branch_state(branch)?;
        Ok(BranchInfo {
            origin: state.origin,
            version: state.version,
            depth: state.depth,
        })
    }

    fn create(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
        _budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError> {
        if cells.is_empty() {
            return Err(StoreError::InvalidArgument);
        }
        for (name, cell) in &cells {
            validate_cell_name(name)?;
            cell.validate()?;
        }

        let state = self.begin_write(branch)?;
        if state.state.contains_key(&key) {
            return Err(StoreError::AlreadyExists);
        }
        state.state.insert(
            key,
            RecordData {
                version: RecordVersion(1),
                cells: cells.into_iter().collect(),
            },
        );
        Ok(with_free_receipt(key))
    }

    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        let found = self
            .state_for_target(target)?
            .get(&key)
            .map(|record| record.to_record(key, projection));
        Ok(with_free_receipt(found))
    }

    fn patch(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        _budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError> {
        for (name, change) in &changes {
            validate_cell_name(name)?;
            if let CellChange::Set(cell) = change {
                cell.validate()?;
            }
        }

        let state = self.begin_write(branch)?;
        let record = state.state.get_mut(&key).ok_or(StoreError::NotFound)?;
        if expected_version.is_some_and(|expected| expected != record.version) {
            return Err(StoreError::Conflict);
        }

        // Staged against a copy first: "may not remove the last cell" must
        // leave the record untouched when it trips, not half-patched.
        let mut staged = record.cells.clone();
        for (name, change) in changes {
            match change {
                CellChange::Set(cell) => {
                    staged.insert(name, cell);
                }
                CellChange::Remove => {
                    staged.remove(&name);
                }
            }
        }
        if staged.is_empty() {
            return Err(StoreError::InvalidArgument);
        }

        record.cells = staged;
        record.version = RecordVersion(record.version.0 + 1);
        Ok(with_free_receipt(record.version))
    }

    fn delete(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        _budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        let state = self.begin_write(branch)?;
        let record = state.state.get(&key).ok_or(StoreError::NotFound)?;
        if expected_version.is_some_and(|expected| expected != record.version) {
            return Err(StoreError::Conflict);
        }
        state.state.remove(&key);
        Ok(with_free_receipt(()))
    }

    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        _budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        let state = &self
            .commit_snapshot(at.unwrap_or_else(|| self.head()))?
            .state;
        let mut matched = evaluate_filter(state, &query.filter)?;

        if let Some(sort) = &query.sort {
            sort_keys_by_cell(state, &mut matched, sort);
        }

        // Counted before paging: `total_matched` is the size of the match, not
        // of the page.
        let total_matched = query.total_matched.then_some(matched.len() as u64);
        let records = matched
            .into_iter()
            .skip(query.page.offset as usize)
            .take(query.page.limit as usize)
            .map(|key| state[&key].to_record(key, query.projection.as_deref()))
            .collect();

        Ok(with_free_receipt(QueryResult {
            records,
            total_matched,
        }))
    }

    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        _budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        let state = &self
            .commit_snapshot(at.unwrap_or_else(|| self.head()))?
            .state;
        Ok(with_free_receipt(
            evaluate_filter(state, filter)?.len() as u64
        ))
    }

    fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError> {
        Ok(content_digest(&self.branch_state(branch)?.state))
    }
}

impl StoreExt for MemStore {
    fn commit_tagged(&mut self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        let committed = self.commit(root)?;
        self.commits[committed.0 as usize].tag = Some(tag);
        Ok(committed)
    }

    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        Ok(self
            .commits
            .iter()
            .position(|snapshot| snapshot.tag == Some(tag))
            .map(|index| CommitId(index as u64)))
    }

    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        Ok(self.commit_snapshot(commit)?.changes.clone())
    }

    fn apply(
        &mut self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        // All-or-nothing: keep the pre-batch branch, and put it back untouched
        // if any op fails. Batching must be a transport optimization, never a
        // semantic one.
        let saved = self.branch_state(branch)?.clone();
        let mut outcomes = Vec::with_capacity(ops.len());

        for op in ops {
            let outcome = match op {
                WriteOp::Create { key, cells } => self
                    .create(branch, key, cells, budget)
                    .map(|_| WriteOutcome::Created),
                WriteOp::Patch {
                    key,
                    expected_version,
                    changes,
                } => self
                    .patch(branch, key, expected_version, changes, budget)
                    .map(|version| WriteOutcome::Patched(version.into_value())),
                WriteOp::Delete {
                    key,
                    expected_version,
                } => self
                    .delete(branch, key, expected_version, budget)
                    .map(|_| WriteOutcome::Deleted),
            };
            match outcome {
                Ok(applied) => outcomes.push(applied),
                Err(error) => {
                    self.restore_branch(branch, saved);
                    return Err(error);
                }
            }
        }

        // One batch, one version bump — not one per op, which is what the
        // per-call increments above would otherwise leave behind.
        let state = self.branch_state_mut(branch)?;
        state.version = BranchVersion(saved.version.0 + 1);
        Ok(with_free_receipt(outcomes))
    }

    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        Ok(self.commit_snapshot(commit)?.hash)
    }

    fn retention(&self) -> (CommitId, CommitId) {
        // Nothing is ever pruned here, so the window is the whole history.
        (CommitId::GENESIS, self.head())
    }

    fn get_many(
        &self,
        target: ReadTarget,
        keys: &[RecordKey],
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError> {
        let state = self.state_for_target(target)?;
        let found = keys
            .iter()
            .map(|key| {
                state
                    .get(key)
                    .map(|record| record.to_record(*key, projection))
            })
            .collect();
        Ok(with_free_receipt(found))
    }
}

// ---------------------------------------------------------------------------
// query evaluation
// ---------------------------------------------------------------------------

/// Evaluate an ordered DNF and return the matching keys, ascending.
///
/// Groups are evaluated in submitted order and unioned; a record matching
/// several groups appears once. Ascending key order falls out of the ordered
/// map, and is both the default result order and the tie-break under a sort.
///
/// An empty filter matches every record.
fn evaluate_filter(
    state: &BTreeMap<RecordKey, RecordData>,
    filter: &Filter,
) -> Result<Vec<RecordKey>, StoreError> {
    if filter.0.is_empty() {
        return Ok(state.keys().copied().collect());
    }

    let mut matched: BTreeMap<RecordKey, ()> = BTreeMap::new();
    for group in &filter.0 {
        for (key, record) in state {
            if record_matches_group(record, group)? {
                matched.insert(*key, ());
            }
        }
    }
    Ok(matched.into_keys().collect())
}

/// Whether every predicate in one AND-group holds for `record`.
///
/// Predicates are intersected in submitted order and the group stops at the
/// first miss — the early exit the spec makes the caller's cost lever.
fn record_matches_group(record: &RecordData, group: &AndGroup) -> Result<bool, StoreError> {
    for predicate in &group.0 {
        if !record_matches_predicate(record, predicate)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether one predicate holds for `record`.
///
/// A predicate only ever sees an **attribute of its own type**: fields are
/// invisible to queries, and the type is part of the index bucket, so an
/// attribute of some other type is a miss rather than a coercion.
///
/// Returns [`StoreError::InvalidQuery`] if the operation is not one the
/// attribute's type can be indexed for.
fn record_matches_predicate(
    record: &RecordData,
    predicate: &Predicate,
) -> Result<bool, StoreError> {
    if !predicate.op.permitted_by(predicate.type_id.index_class()) {
        return Err(StoreError::InvalidQuery);
    }

    let hit = match record.cells.get(&predicate.cell) {
        Some(cell)
            if matches!(cell.kind, CellKind::Attribute) && cell.type_id == predicate.type_id =>
        {
            compare_values(
                &cell.value,
                &predicate.value,
                predicate.op,
                predicate.type_id,
            )
        }
        _ => false,
    };
    Ok(hit != predicate.negated)
}

/// Compare a stored value against a predicate's operand.
///
/// Equality and prefix work on the canonical bytes directly; ordering works on
/// the type's order encoding, which for signed types is not the same thing.
fn compare_values(stored: &[u8], operand: &[u8], op: CompareOp, type_id: TypeId) -> bool {
    match op {
        CompareOp::Prefix => stored.starts_with(operand),
        CompareOp::Eq => stored == operand,
        CompareOp::Lt | CompareOp::Lte | CompareOp::Gt | CompareOp::Gte => {
            let left = order_encoding(stored, type_id);
            let right = order_encoding(operand, type_id);
            match op {
                CompareOp::Lt => left < right,
                CompareOp::Lte => left <= right,
                CompareOp::Gt => left > right,
                CompareOp::Gte => left >= right,
                CompareOp::Eq | CompareOp::Prefix => unreachable!("handled above"),
            }
        }
    }
}

/// The spec's `enc`: an encoding whose lexicographic byte order equals the
/// type's domain order.
///
/// Unsigned big-endian types already satisfy it. Signed ones do not — two's
/// complement puts negatives above positives bytewise — so their sign bit is
/// flipped, which is what makes range scans and sorting meaningful on a plain
/// sorted key-value store.
fn order_encoding(value: &[u8], type_id: TypeId) -> Vec<u8> {
    let mut encoded = value.to_vec();
    let signed = type_id == TypeId::I32 || type_id == TypeId::DEC;
    if signed && !encoded.is_empty() {
        encoded[0] ^= 0x80;
    }
    encoded
}

/// Sort matched keys by one cell, in the requested direction.
///
/// Records lacking the sort cell sort last, and ascending key order is the
/// tie-break — so the ordering is total and two implementations agreeing on
/// the match set also agree on the page.
fn sort_keys_by_cell(state: &BTreeMap<RecordKey, RecordData>, keys: &mut [RecordKey], sort: &Sort) {
    let sort_value = |key: &RecordKey| {
        state[key]
            .cells
            .get(&sort.cell)
            .map(|cell| order_encoding(&cell.value, cell.type_id))
    };

    keys.sort_by(|left, right| {
        let ordering = match (sort_value(left), sort_value(right)) {
            (Some(left_value), Some(right_value)) => left_value.cmp(&right_value),
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (None, None) => core::cmp::Ordering::Equal,
        };
        let ordering = ordering.then_with(|| left.cmp(right));
        match sort.direction {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });
}

// ---------------------------------------------------------------------------
// changesets and the digest
// ---------------------------------------------------------------------------

/// The before/after pairs describing how `after` differs from `before`.
///
/// Compares **cells only**: a record whose version moved but whose content did
/// not is not a change, which keeps changesets consistent with the digest.
/// Ordered by key, so the output is deterministic.
fn record_changes_between(
    before: &BTreeMap<RecordKey, RecordData>,
    after: &BTreeMap<RecordKey, RecordData>,
) -> Vec<RecordChange> {
    let mut changes = Vec::new();

    for (key, previous) in before {
        match after.get(key) {
            Some(current) if current.cells == previous.cells => {}
            Some(current) => changes.push(RecordChange {
                key: *key,
                before: Some(previous.to_record(*key, None)),
                after: Some(current.to_record(*key, None)),
            }),
            None => changes.push(RecordChange {
                key: *key,
                before: Some(previous.to_record(*key, None)),
                after: None,
            }),
        }
    }

    for (key, current) in after {
        if !before.contains_key(key) {
            changes.push(RecordChange {
                key: *key,
                before: None,
                after: Some(current.to_record(*key, None)),
            });
        }
    }

    changes.sort_by_key(|change| change.key);
    changes
}

/// A deterministic digest over logical record content.
///
/// **Not a commitment.** FNV-1a over the canonical encoding, run with four
/// seeds to fill 32 bytes. It detects divergence between implementations,
/// which is what the conformance suite needs, and nothing more: no collision
/// resistance, no proofs, no incremental maintenance. See the crate docs.
///
/// Excludes `#version`, as [`Store::branch_hash`] requires — the input is
/// built from cells alone, so a write leaving content unchanged leaves the
/// digest unchanged. Lengths are framed so that neighbouring names and values
/// cannot be confused for one another.
fn content_digest(state: &BTreeMap<RecordKey, RecordData>) -> [u8; 32] {
    let mut encoded = Vec::new();
    for (key, record) in state {
        encoded.extend_from_slice(&key.0);
        for (name, cell) in &record.cells {
            encoded.extend_from_slice(&(name.len() as u32).to_be_bytes());
            encoded.extend_from_slice(name.as_bytes());
            encoded.push(cell.tag());
            encoded.extend_from_slice(&(cell.value.len() as u32).to_be_bytes());
            encoded.extend_from_slice(&cell.value);
        }
    }

    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    const SEED_STRIDE: u64 = 0x9E37_79B9_7F4A_7C15;

    let mut digest = [0u8; 32];
    for (lane, chunk) in digest.chunks_mut(8).enumerate() {
        let mut accumulator = FNV_OFFSET_BASIS ^ (lane as u64).wrapping_mul(SEED_STRIDE);
        for byte in &encoded {
            accumulator ^= *byte as u64;
            accumulator = accumulator.wrapping_mul(FNV_PRIME);
        }
        chunk.copy_from_slice(&accumulator.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference implementation satisfies the whole spec contract.
    #[test]
    fn store_conformance() {
        arkiv_interfaces::store::conformance::run_all(&MemStore::new);
    }

    /// And the host-facing extensions on top of it.
    #[test]
    fn store_ext_conformance() {
        arkiv_interfaces::store::conformance::run_all_ext(&MemStore::new);
    }

    /// A 20-byte address survives the trip into the 32-byte key space, and an
    /// entity key is not mistaken for a padded address on the way back.
    #[test]
    fn address_keys_round_trip() {
        let address = [0xAB; 20];
        let key = RecordKey::from_address(address);
        assert_eq!(key.as_address(), Some(address));
        assert_eq!(RecordKey::from_entity([0xCD; 32]).as_address(), None);
    }

    /// The tag byte carries kind and type together and splits back cleanly,
    /// which is what makes stored cells self-describing.
    #[test]
    fn tag_byte_round_trips() {
        for type_id in [TypeId::U64, TypeId::STR, TypeId::ADDR] {
            for cell in [
                Cell::attribute(type_id, alloc::vec![1]),
                Cell::field(type_id, alloc::vec![1]),
            ] {
                assert_eq!(Cell::split_tag(cell.tag()), Some((cell.kind, type_id)));
            }
        }
        assert_eq!(Cell::split_tag(0x00), None, "type id 0 is reserved");
    }
}
