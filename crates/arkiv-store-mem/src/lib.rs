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
//! - **`fork` copies the parent's diff** instead of layering a new one. The
//!   real store makes `fork` O(1); here O(n) buys a trivially correct read
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
    AndGroup, BranchId, BranchInfo, BranchVersion, Budget, Cell, CellChange, CellName, CommitId,
    CompareOp, Filter, Metered, Origin, Predicate, Query, QueryResult, ReadTarget, Receipt, Record,
    RecordChange, RecordKey, RecordVersion, Sort, SortDirection, Store, StoreError, StoreExt,
    TypeId, WriteOp, WriteOutcome, validate_cell_name,
};

/// One record's stored form: its cells, plus the `#version` meta entry the
/// spec keeps alongside them. Presence of this struct *is* record existence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct RecordData {
    version: RecordVersion,
    cells: BTreeMap<CellName, Cell>,
}

impl RecordData {
    fn to_record(&self, key: RecordKey, projection: Option<&[CellName]>) -> Record {
        let cells = self
            .cells
            .iter()
            .filter(|(name, _)| projection.is_none_or(|p| p.contains(name)))
            .map(|(name, cell)| (name.clone(), cell.clone()))
            .collect();
        Record {
            key,
            version: self.version,
            cells,
        }
    }
}

/// The committed state as of one commit, plus what getting here changed.
#[derive(Debug, Clone)]
struct Snapshot {
    state: BTreeMap<RecordKey, RecordData>,
    hash: [u8; 32],
    tag: Option<[u8; 32]>,
    changes: Vec<RecordChange>,
}

#[derive(Debug, Clone)]
struct BranchState {
    origin: Origin,
    version: BranchVersion,
    depth: u32,
    /// The branch's view of the world: its origin's state with its own writes
    /// applied. See the crate docs on why this is a copy, not a layer.
    state: BTreeMap<RecordKey, RecordData>,
}

/// An in-memory [`Store`]. See the crate docs.
#[derive(Debug)]
pub struct MemStore {
    commits: Vec<Snapshot>,
    branches: BTreeMap<u64, BranchState>,
    next_branch: u64,
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemStore {
    pub fn new() -> Self {
        let genesis = BTreeMap::new();
        Self {
            commits: alloc::vec![Snapshot {
                hash: digest(&genesis),
                state: genesis,
                tag: None,
                changes: Vec::new(),
            }],
            branches: BTreeMap::new(),
            next_branch: 0,
        }
    }

    fn branch(&self, id: BranchId) -> Result<&BranchState, StoreError> {
        self.branches.get(&id.0).ok_or(StoreError::HandleInvalid)
    }

    fn branch_mut(&mut self, id: BranchId) -> Result<&mut BranchState, StoreError> {
        self.branches
            .get_mut(&id.0)
            .ok_or(StoreError::HandleInvalid)
    }

    fn snapshot(&self, id: CommitId) -> Result<&Snapshot, StoreError> {
        self.commits.get(id.0 as usize).ok_or(StoreError::NotFound)
    }

    /// The state a read resolves against.
    fn state_of(&self, target: ReadTarget) -> Result<&BTreeMap<RecordKey, RecordData>, StoreError> {
        match target {
            ReadTarget::Branch(b) => Ok(&self.branch(b)?.state),
            ReadTarget::Commit(c) => Ok(&self.snapshot(c)?.state),
        }
    }

    /// Every write lands here: bump the branch's version, then mutate.
    fn writable(&mut self, id: BranchId) -> Result<&mut BranchState, StoreError> {
        let branch = self.branch_mut(id)?;
        branch.version = BranchVersion(branch.version.0 + 1);
        Ok(branch)
    }
}

/// Nothing here meters, so every receipt is the same zero. Cost is the real
/// store's concern; modelling it here would invent numbers that mean nothing.
const fn free() -> Receipt {
    Receipt {
        cost: arkiv_interfaces::store::CostUnits(0),
        schedule: arkiv_interfaces::store::ScheduleVersion(0),
    }
}

fn metered<T>(value: T) -> Metered<T> {
    Metered::new(value, free())
}

impl Store for MemStore {
    fn head(&self) -> CommitId {
        CommitId(self.commits.len() as u64 - 1)
    }

    fn begin(&mut self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        let origin = at.unwrap_or_else(|| self.head());
        let state = self.snapshot(origin)?.state.clone();

        let id = BranchId(self.next_branch);
        self.next_branch += 1;
        self.branches.insert(
            id.0,
            BranchState {
                origin: Origin::Commit(origin),
                version: BranchVersion(0),
                depth: 0,
                state,
            },
        );
        Ok(id)
    }

    fn fork(&mut self, parent: BranchId) -> Result<BranchId, StoreError> {
        let p = self.branch(parent)?;
        let child = BranchState {
            origin: Origin::Branch(parent, p.version),
            version: BranchVersion(0),
            depth: p.depth + 1,
            state: p.state.clone(),
        };

        let id = BranchId(self.next_branch);
        self.next_branch += 1;
        self.branches.insert(id.0, child);
        Ok(id)
    }

    fn merge(&mut self, child: BranchId) -> Result<BranchVersion, StoreError> {
        let c = self.branch(child)?.clone();
        let Origin::Branch(parent, forked_at) = c.origin else {
            // A root branch commits; it does not merge.
            return Err(StoreError::HandleInvalid);
        };

        // The guard: the parent must not have moved since the fork. In
        // blockchain mode this never fires — it asserts the sequential
        // discipline rather than resolving contention.
        if self.branch(parent)?.version != forked_at {
            return Err(StoreError::Conflict);
        }

        self.branches.remove(&child.0);
        let p = self.branch_mut(parent)?;
        p.state = c.state;
        p.version = BranchVersion(p.version.0 + 1);
        Ok(p.version)
    }

    fn discard(&mut self, branch: BranchId) -> Result<(), StoreError> {
        if self.branches.remove(&branch.0).is_none() {
            return Err(StoreError::HandleInvalid);
        }
        // Descendants are grounded on a handle that no longer exists, so drop
        // them too — the spec's "and its open descendants".
        let orphans: Vec<u64> = self
            .branches
            .iter()
            .filter(|(_, b)| matches!(b.origin, Origin::Branch(p, _) if p == branch))
            .map(|(id, _)| *id)
            .collect();
        for id in orphans {
            self.discard(BranchId(id))?;
        }
        Ok(())
    }

    fn commit(&mut self, root: BranchId) -> Result<CommitId, StoreError> {
        let b = self.branch(root)?.clone();
        let Origin::Commit(origin) = b.origin else {
            // Child branches may only merge or discard.
            return Err(StoreError::HandleInvalid);
        };
        if origin != self.head() {
            return Err(StoreError::Conflict);
        }

        let before = &self.snapshot(origin)?.state;
        let changes = diff(before, &b.state);
        let hash = digest(&b.state);

        self.branches.remove(&root.0);
        self.commits.push(Snapshot {
            state: b.state,
            hash,
            tag: None,
            changes,
        });
        Ok(self.head())
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        let b = self.branch(branch)?;
        Ok(BranchInfo {
            origin: b.origin,
            version: b.version,
            depth: b.depth,
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

        let b = self.writable(branch)?;
        if b.state.contains_key(&key) {
            return Err(StoreError::AlreadyExists);
        }
        b.state.insert(
            key,
            RecordData {
                version: RecordVersion(1),
                cells: cells.into_iter().collect(),
            },
        );
        Ok(metered(key))
    }

    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        let found = self
            .state_of(target)?
            .get(&key)
            .map(|r| r.to_record(key, projection));
        Ok(metered(found))
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

        let b = self.writable(branch)?;
        let record = b.state.get_mut(&key).ok_or(StoreError::NotFound)?;
        if expected_version.is_some_and(|v| v != record.version) {
            return Err(StoreError::Conflict);
        }

        // Applied to a copy first: "may not remove the last cell" must leave
        // the record untouched when it trips, not half-patched.
        let mut next = record.cells.clone();
        for (name, change) in changes {
            match change {
                CellChange::Set(cell) => {
                    next.insert(name, cell);
                }
                CellChange::Remove => {
                    next.remove(&name);
                }
            }
        }
        if next.is_empty() {
            return Err(StoreError::InvalidArgument);
        }

        record.cells = next;
        record.version = RecordVersion(record.version.0 + 1);
        Ok(metered(record.version))
    }

    fn delete(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        _budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        let b = self.writable(branch)?;
        let record = b.state.get(&key).ok_or(StoreError::NotFound)?;
        if expected_version.is_some_and(|v| v != record.version) {
            return Err(StoreError::Conflict);
        }
        b.state.remove(&key);
        Ok(metered(()))
    }

    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        _budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        let state = &self.snapshot(at.unwrap_or_else(|| self.head()))?.state;
        let mut matched = evaluate(state, &query.filter)?;

        if let Some(sort) = &query.sort {
            sort_matched(state, &mut matched, sort);
        }

        let total = query.total_matched.then_some(matched.len() as u64);
        let records = matched
            .into_iter()
            .skip(query.page.offset as usize)
            .take(query.page.limit as usize)
            .map(|k| state[&k].to_record(k, query.projection.as_deref()))
            .collect();

        Ok(metered(QueryResult {
            records,
            total_matched: total,
        }))
    }

    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        _budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        let state = &self.snapshot(at.unwrap_or_else(|| self.head()))?.state;
        Ok(metered(evaluate(state, filter)?.len() as u64))
    }

    fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError> {
        Ok(digest(&self.branch(branch)?.state))
    }
}

impl StoreExt for MemStore {
    fn commit_tagged(&mut self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        let id = self.commit(root)?;
        self.commits[id.0 as usize].tag = Some(tag);
        Ok(id)
    }

    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        Ok(self
            .commits
            .iter()
            .position(|s| s.tag == Some(tag))
            .map(|i| CommitId(i as u64)))
    }

    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        Ok(self.snapshot(commit)?.changes.clone())
    }

    fn apply(
        &mut self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        // All-or-nothing: stage against a copy, swap it in only if every op
        // succeeded. Batching must be a transport optimization, never a
        // semantic one.
        let restore = self.branch(branch)?.clone();
        let mut outcomes = Vec::with_capacity(ops.len());

        for op in ops {
            let result = match op {
                WriteOp::Create { key, cells } => self
                    .create(branch, key, cells, budget)
                    .map(|_| WriteOutcome::Created),
                WriteOp::Patch {
                    key,
                    expected_version,
                    changes,
                } => self
                    .patch(branch, key, expected_version, changes, budget)
                    .map(|v| WriteOutcome::Patched(v.into_value())),
                WriteOp::Delete {
                    key,
                    expected_version,
                } => self
                    .delete(branch, key, expected_version, budget)
                    .map(|_| WriteOutcome::Deleted),
            };
            match result {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    self.branches.insert(branch.0, restore);
                    return Err(e);
                }
            }
        }

        // One batch, one version bump — not one per op.
        let b = self.branch_mut(branch)?;
        b.version = BranchVersion(restore.version.0 + 1);
        Ok(metered(outcomes))
    }

    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        Ok(self.snapshot(commit)?.hash)
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
        let state = self.state_of(target)?;
        let found = keys
            .iter()
            .map(|k| state.get(k).map(|r| r.to_record(*k, projection)))
            .collect();
        Ok(metered(found))
    }
}

// ---------------------------------------------------------------------------
// query evaluation
// ---------------------------------------------------------------------------

/// Evaluate an ordered DNF, in submitted order, deduplicating across groups.
/// Returns keys in ascending order — the default result order, and the
/// tie-break under a sort.
fn evaluate(
    state: &BTreeMap<RecordKey, RecordData>,
    filter: &Filter,
) -> Result<Vec<RecordKey>, StoreError> {
    // An empty filter matches everything; an empty group matches everything
    // within it. Both fall out of the fold below.
    let mut matched: BTreeMap<RecordKey, ()> = BTreeMap::new();
    for group in &filter.0 {
        for (key, record) in state {
            if matches_group(record, group)? {
                matched.insert(*key, ());
            }
        }
    }
    if filter.0.is_empty() {
        return Ok(state.keys().copied().collect());
    }
    Ok(matched.into_keys().collect())
}

fn matches_group(record: &RecordData, group: &AndGroup) -> Result<bool, StoreError> {
    for predicate in &group.0 {
        if !matches_predicate(record, predicate)? {
            // Early exit: a group stops as soon as its intermediate is empty.
            return Ok(false);
        }
    }
    Ok(true)
}

fn matches_predicate(record: &RecordData, p: &Predicate) -> Result<bool, StoreError> {
    if !p.op.permitted_by(p.type_id.index_class()) {
        return Err(StoreError::InvalidQuery);
    }

    let hit = match record.cells.get(&p.cell) {
        // A field is invisible to queries; so is an attribute of another type
        // (the type is part of the bucket).
        Some(cell)
            if matches!(cell.kind, arkiv_interfaces::store::CellKind::Attribute)
                && cell.type_id == p.type_id =>
        {
            compare(&cell.value, &p.value, p.op, p.type_id)
        }
        _ => false,
    };
    Ok(hit != p.negated)
}

fn compare(value: &[u8], operand: &[u8], op: CompareOp, type_id: TypeId) -> bool {
    if op == CompareOp::Prefix {
        return value.starts_with(operand);
    }
    if op == CompareOp::Eq {
        return value == operand;
    }
    // Ordering is defined on the type's order-preserving encoding, not on the
    // raw canonical bytes — for signed types those differ.
    let (a, b) = (enc(value, type_id), enc(operand, type_id));
    match op {
        CompareOp::Lt => a < b,
        CompareOp::Lte => a <= b,
        CompareOp::Gt => a > b,
        CompareOp::Gte => a >= b,
        CompareOp::Eq | CompareOp::Prefix => unreachable!("handled above"),
    }
}

/// The order-encoding `enc`: lexicographic byte order must equal domain order.
/// Unsigned big-endian types already satisfy it; signed ones need the sign bit
/// flipped so negatives sort below positives.
fn enc(value: &[u8], type_id: TypeId) -> Vec<u8> {
    let mut out = value.to_vec();
    if (type_id == TypeId::I32 || type_id == TypeId::DEC) && !out.is_empty() {
        out[0] ^= 0x80;
    }
    out
}

fn sort_matched(state: &BTreeMap<RecordKey, RecordData>, keys: &mut [RecordKey], sort: &Sort) {
    keys.sort_by(|a, b| {
        let pick = |k: &RecordKey| {
            state[k]
                .cells
                .get(&sort.cell)
                .map(|c| enc(&c.value, c.type_id))
        };
        // Records lacking the sort key sort last, then by key — so the order
        // is total and the tie-break is the default key order.
        let ordering = match (pick(a), pick(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (None, None) => core::cmp::Ordering::Equal,
        };
        let ordering = ordering.then_with(|| a.cmp(b));
        match sort.direction {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });
}

// ---------------------------------------------------------------------------
// changesets and the digest
// ---------------------------------------------------------------------------

fn diff(
    before: &BTreeMap<RecordKey, RecordData>,
    after: &BTreeMap<RecordKey, RecordData>,
) -> Vec<RecordChange> {
    let mut out = Vec::new();
    for (key, old) in before {
        match after.get(key) {
            Some(new) if new.cells == old.cells => {}
            Some(new) => out.push(RecordChange {
                key: *key,
                before: Some(old.to_record(*key, None)),
                after: Some(new.to_record(*key, None)),
            }),
            None => out.push(RecordChange {
                key: *key,
                before: Some(old.to_record(*key, None)),
                after: None,
            }),
        }
    }
    for (key, new) in after {
        if !before.contains_key(key) {
            out.push(RecordChange {
                key: *key,
                before: None,
                after: Some(new.to_record(*key, None)),
            });
        }
    }
    out.sort_by_key(|c| c.key);
    out
}

/// A deterministic digest over logical record content.
///
/// **Not a commitment.** FNV-1a over the canonical encoding, four seeds wide.
/// It detects divergence between implementations, which is what the
/// conformance suite needs, and nothing more: no collision resistance, no
/// proofs. See the crate docs.
///
/// Excludes `#version`, exactly as [`Store::branch_hash`] requires — the input
/// is built from cells alone, so a write that leaves content unchanged leaves
/// the digest unchanged.
fn digest(state: &BTreeMap<RecordKey, RecordData>) -> [u8; 32] {
    let mut buf = Vec::new();
    for (key, record) in state {
        buf.extend_from_slice(&key.0);
        for (name, cell) in &record.cells {
            buf.extend_from_slice(&(name.len() as u32).to_be_bytes());
            buf.extend_from_slice(name.as_bytes());
            buf.push(cell.tag());
            buf.extend_from_slice(&(cell.value.len() as u32).to_be_bytes());
            buf.extend_from_slice(&cell.value);
        }
    }

    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h: u64 = 0xcbf29ce484222325 ^ (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
        for byte in &buf {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        chunk.copy_from_slice(&h.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_conformance() {
        arkiv_interfaces::store::conformance::run_all(&MemStore::new);
    }

    #[test]
    fn store_ext_conformance() {
        arkiv_interfaces::store::conformance::run_all_ext(&MemStore::new);
    }

    #[test]
    fn address_keys_round_trip() {
        let address = [0xAB; 20];
        let key = RecordKey::from_address(address);
        assert_eq!(key.as_address(), Some(address));
        // An entity address is not a padded account address.
        assert_eq!(RecordKey::from_entity([0xCD; 32]).as_address(), None);
    }

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
