//! A test store that prices its calls and can fail one on purpose.
//!
//! The in-memory reference store prices nothing, so every receipt cost is
//! zero, and code that dropped a receipt would look the same as code that kept
//! it. [`Priced`] wraps any store and gives each CRUD call kind a cost of its
//! own; with powers of ten, a total spells out which calls it is made of. Write
//! details and everything else come from the wrapped store unchanged.
//!
//! It can also play a faulty store: return records without their size, or
//! fail one chosen write with [`StoreError::Internal`], for the paths that must
//! survive a fault.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{
    BranchId, BranchInfo, Budget, Cell, CellChange, CellName, CommitId, CostUnits, Filter, Metered,
    Query, QueryResult, ReadTarget, Receipt, Record, RecordChange, RecordKey, RecordVersion,
    ScheduleVersion, SealedCommit, SegmentAddress, SegmentKey, SegmentOrdinal, SegmentRow, Store,
    StoreError, StoreExt, WriteOp, WriteOutcome,
};

/// What each CRUD call kind costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prices {
    pub get: u64,
    pub create: u64,
    pub patch: u64,
    pub delete: u64,
}

impl Prices {
    /// One power of ten per kind, so a sum of costs counts the calls behind it.
    pub const DISTINCT: Self = Self {
        get: 1,
        create: 10,
        patch: 100,
        delete: 1_000,
    };
}

/// The schedule every priced receipt names.
pub const SCHEDULE: ScheduleVersion = ScheduleVersion(7);

/// The pricing commit every priced receipt names.
pub const PRICED_AT: CommitId = CommitId(42);

/// `inner`, with every CRUD receipt's cost set from [`Prices`].
#[derive(Debug)]
pub struct Priced<S> {
    pub inner: S,
    pub prices: Prices,
    /// Return records without their size, as a faulty store would.
    pub strip_meta: bool,
    /// Fail the write (create, patch or delete) with this 0-based index.
    pub failing_write: Option<u64>,
    writes: AtomicU64,
}

impl<S> Priced<S> {
    pub const fn new(inner: S, prices: Prices) -> Self {
        Self {
            inner,
            prices,
            strip_meta: false,
            failing_write: None,
            writes: AtomicU64::new(0),
        }
    }

    /// Return records without their size.
    pub fn without_meta(mut self) -> Self {
        self.strip_meta = true;
        self
    }

    /// Fail the write with this 0-based index, counting creates, patches and
    /// deletes together.
    pub fn failing_write(mut self, index: u64) -> Self {
        self.failing_write = Some(index);
        self
    }

    /// Refuse this write, and only this one, as a store fault.
    fn fault(&self) -> Result<(), StoreError> {
        let index = self.writes.fetch_add(1, Ordering::Relaxed);
        match self.failing_write {
            Some(failing) if failing == index => Err(StoreError::Internal),
            _ => Ok(()),
        }
    }
}

fn priced(receipt: Receipt, cost: u64) -> Receipt {
    Receipt {
        cost: CostUnits(cost),
        schedule: SCHEDULE,
        priced_at: Some(PRICED_AT),
        ..receipt
    }
}

/// Only the four CRUD calls change anything; everything else forwards.
impl<S: Store> Store for Priced<S> {
    fn head(&self) -> CommitId {
        self.inner.head()
    }
    fn begin(&self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        self.inner.begin(at)
    }
    fn checkpoint(&self, branch: BranchId) -> Result<(), StoreError> {
        self.inner.checkpoint(branch)
    }
    fn rollback(&self, branch: BranchId) -> Result<(), StoreError> {
        self.inner.rollback(branch)
    }
    fn seal(&self, branch: BranchId) -> Result<SealedCommit, StoreError> {
        self.inner.seal(branch)
    }
    fn discard(&self, branch: BranchId) -> Result<(), StoreError> {
        self.inner.discard(branch)
    }
    fn commit(&self, root: BranchId) -> Result<CommitId, StoreError> {
        self.inner.commit(root)
    }
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        self.inner.branch_info(branch)
    }

    fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError> {
        self.fault()?;
        let created = self.inner.create(branch, key, cells, budget)?;
        Ok(Metered::new(
            created.value,
            priced(created.receipt, self.prices.create),
        ))
    }
    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        let read = self.inner.get(target, key, projection, budget)?;
        let mut record = read.value;
        if self.strip_meta
            && let Some(record) = record.as_mut()
        {
            record.meta = None;
        }
        Ok(Metered::new(record, priced(read.receipt, self.prices.get)))
    }
    fn patch(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError> {
        self.fault()?;
        let patched = self
            .inner
            .patch(branch, key, expected_version, changes, budget)?;
        Ok(Metered::new(
            patched.value,
            priced(patched.receipt, self.prices.patch),
        ))
    }
    fn delete(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        self.fault()?;
        let deleted = self.inner.delete(branch, key, expected_version, budget)?;
        Ok(Metered::new(
            (),
            priced(deleted.receipt, self.prices.delete),
        ))
    }

    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        self.inner.query(at, query, budget)
    }
    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        self.inner.count(at, filter, budget)
    }
    fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError> {
        self.inner.branch_hash(branch)
    }

    fn segment_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<SegmentKey>,
        row: SegmentRow,
    ) -> Result<SegmentOrdinal, StoreError> {
        self.inner.segment_append(branch, segment, key, row)
    }
    fn segment_append_many(
        &self,
        branch: BranchId,
        writes: &[(&str, Option<SegmentKey>, SegmentRow)],
    ) -> Result<Vec<SegmentOrdinal>, StoreError> {
        self.inner.segment_append_many(branch, writes)
    }
    fn segment_get(
        &self,
        segment: &str,
        address: SegmentAddress,
    ) -> Result<Option<SegmentRow>, StoreError> {
        self.inner.segment_get(segment, address)
    }
    fn segment_ordinal_of(
        &self,
        segment: &str,
        key: SegmentKey,
    ) -> Result<Option<SegmentOrdinal>, StoreError> {
        self.inner.segment_ordinal_of(segment, key)
    }
    fn segment_range_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<core::ops::Range<SegmentOrdinal>, StoreError> {
        self.inner.segment_range_of(segment, commit)
    }
    fn segment_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<Vec<SegmentRow>, StoreError> {
        self.inner.segment_rows_of(segment, commit)
    }
}

/// So a priced store can sit under a host view, which needs `StoreExt`. The
/// batch calls forward unpriced: nothing that meters goes through them.
impl<S: StoreExt> StoreExt for Priced<S> {
    fn commit_tagged(&self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        self.inner.commit_tagged(root, tag)
    }
    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        self.inner.commit_by_tag(tag)
    }
    fn tag_of(&self, commit: CommitId) -> Result<Option<[u8; 32]>, StoreError> {
        self.inner.tag_of(commit)
    }
    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        self.inner.changes(commit)
    }
    fn apply(
        &self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        self.inner.apply(branch, ops, budget)
    }
    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        self.inner.commit_hash(commit)
    }
    fn retention(&self) -> (CommitId, CommitId) {
        self.inner.retention()
    }
    fn get_many(
        &self,
        target: ReadTarget,
        keys: &[RecordKey],
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError> {
        self.inner.get_many(target, keys, projection, budget)
    }
}
