//! [`ValkeyStore`] — a Valkey-backed [`Store`].
//!
//! The interim backing store: it stands in for Golem DB until that exists,
//! and it is what exercises the **out-of-process seam**, which an in-process
//! implementation cannot. Held to the same
//! [`conformance`](arkiv_interfaces::store::conformance) suite as everything
//! else, so a host cannot tell it apart from any other conformant store.
//!
//! # What this does not give you
//!
//! **The digest is not a commitment.** `branch_digest` is an XOR fold of
//! per-record FNV-1a digests. That is genuinely incremental — folding a record
//! out and back in is how an update is applied, so the cost is proportional to
//! what changed rather than to the state — and it is order-independent, so two
//! nodes reaching the same content agree. But XOR is not collision-resistant
//! and there are no proofs. It detects divergence; it commits to nothing.
//!
//! This is only safe while anchoring keeps reth's own MPT as the header state
//! root. A store whose digest *becomes* the header root must be the real one.
//!
//! **There is one durability domain too many.** Writes here are not
//! transactional with the host's, and a crash mid-commit can leave the head
//! advanced without its undo log. See [`backend`] for why, and the
//! architecture review for what it costs.
//!
//! # Blocking
//!
//! The [`Store`] trait is synchronous and `fred` is not, so calls block the
//! calling thread — safely from any context, including inside an async runtime
//! (see [`bridge`]). Calling from an async task without `spawn_blocking` will
//! stall that runtime's worker for a round-trip. That is a choice the caller
//! can see and make, not a panic waiting to happen.

#![forbid(unsafe_code)]

mod backend;
mod bridge;
mod codec;
mod keys;

use std::time::{SystemTime, UNIX_EPOCH};

use arkiv_interfaces::store::{
    BranchId, BranchInfo, BranchVersion, Budget, Cell, CellChange, CellName, CommitId, CostUnits,
    Filter, Metered, Query, QueryResult, ReadTarget, Receipt, Record, RecordChange, RecordKey,
    RecordVersion, ScheduleVersion, Sort, SortDirection, Store, StoreError, StoreExt, WriteOp,
    WriteOutcome,
};
use fred::prelude::*;

use crate::backend::Backend;
use crate::bridge::Bridge;
use crate::keys::Namespace;

/// A [`Store`] over a Valkey server.
///
/// Each instance owns a key namespace, so several can share one server
/// without colliding — which is what makes parallel tests and a shared devnet
/// server workable. See the crate docs for what this store does and does not
/// promise.
#[derive(Debug)]
pub struct ValkeyStore {
    bridge: Bridge,
    backend: Backend,
    /// Whether dropping this store should erase its namespace — true only for
    /// [`ephemeral`](Self::ephemeral). See the [`Drop`] impl.
    owns_namespace: bool,
}

impl ValkeyStore {
    /// Connect to `url` and claim `namespace` for this store's keys.
    ///
    /// Existing data under that namespace is adopted as committed state, so
    /// reconnecting to the same namespace resumes where the last run left off.
    /// Branches are not: they are volatile by definition, and any left behind
    /// by a previous run are cleared.
    pub fn connect(url: &str, namespace: &str) -> Result<Self, StoreError> {
        let bridge = Bridge::new().map_err(|_| StoreError::Internal)?;
        let namespace = Namespace::new(namespace);

        let config = Config::from_url(url).map_err(|_| StoreError::Internal)?;
        let client = Client::new(config, None, None, None);
        let connecting = client.clone();
        bridge
            .run(async move { connecting.init().await })
            .map_err(|_| StoreError::Internal)?;

        let store = Self {
            bridge,
            backend: Backend::new(client, namespace),
            owns_namespace: false,
        };
        store.clear_branches()?;
        Ok(store)
    }

    /// Connect to a namespace nothing else is using, and **erase it on drop**.
    ///
    /// For tests: each call gets a fresh, empty state on a shared server, so
    /// assertions cannot see one another's writes, and none of them leave
    /// anything behind. The conformance suite builds a store per assertion, so
    /// without the drop a single run would leave dozens of dead namespaces.
    pub fn ephemeral(url: &str) -> Result<Self, StoreError> {
        // Wall-clock nanos plus the process id: unique enough for tests on one
        // machine, and it needs no dependency.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Internal)?
            .as_nanos();
        let mut store = Self::connect(url, &format!("arkiv-test:{}:{nanos}", std::process::id()))?;
        store.owns_namespace = true;
        Ok(store)
    }

    /// Delete every key in this store's namespace.
    ///
    /// Uses `KEYS`, which scans the whole keyspace — fine for tearing down a
    /// test namespace, and not something to run against a busy server.
    pub fn drop_namespace(&self) -> Result<(), StoreError> {
        let client = self.backend.client().clone();
        let pattern = self.backend.namespace().wildcard();
        self.bridge.run(async move {
            let found: Vec<String> = client
                .custom(fred::cmd!("KEYS"), vec![pattern])
                .await
                .map_err(|_| StoreError::Internal)?;
            if !found.is_empty() {
                let _: u64 = client.del(found).await.map_err(|_| StoreError::Internal)?;
            }
            Ok(())
        })
    }

    /// Drop branches left behind by an earlier run. Branches are volatile by
    /// definition, so surviving a restart would be the bug.
    fn clear_branches(&self) -> Result<(), StoreError> {
        let backend = self.backend.clone();
        self.bridge
            .run(async move { backend.clear_branches().await })
    }

    /// Run one backend operation to completion.
    fn call<T, F, Fut>(&self, operation: F) -> Result<T, StoreError>
    where
        F: FnOnce(Backend) -> Fut,
        Fut: Future<Output = Result<T, StoreError>> + Send + 'static,
        T: Send + 'static,
    {
        self.bridge.run(operation(self.backend.clone()))
    }
}

impl Drop for ValkeyStore {
    /// Erase the namespace, but only for [`ephemeral`](ValkeyStore::ephemeral).
    ///
    /// A [`connect`](ValkeyStore::connect) namespace holds the caller's data —
    /// a node's committed state — and wiping that on shutdown would be a
    /// catastrophe, not a cleanup. Errors are swallowed: a drop cannot report,
    /// and a failed teardown leaves junk rather than corruption.
    fn drop(&mut self) {
        if self.owns_namespace {
            let _ = self.drop_namespace();
        }
    }
}

/// Nothing here meters: cost is the real store's concern, priced against a
/// schedule this one does not have. Inventing numbers would give call sites
/// something that looks like a cost and means nothing.
const fn free_receipt() -> Receipt {
    Receipt {
        cost: CostUnits(0),
        schedule: ScheduleVersion(0),
    }
}

fn with_free_receipt<T>(value: T) -> Metered<T> {
    Metered::new(value, free_receipt())
}

impl Store for ValkeyStore {
    fn head(&self) -> CommitId {
        // The trait's one infallible method. A server that cannot answer is
        // not a state this store can represent, so it reports genesis and the
        // next fallible call surfaces the real error.
        self.call(|backend| async move { backend.head().await })
            .map_or(CommitId::GENESIS, CommitId)
    }

    fn begin(&mut self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        self.call(move |backend| async move { backend.begin(at).await })
    }

    fn fork(&mut self, parent: BranchId) -> Result<BranchId, StoreError> {
        self.call(move |backend| async move { backend.fork(parent).await })
    }

    fn merge(&mut self, child: BranchId) -> Result<BranchVersion, StoreError> {
        self.call(move |backend| async move { backend.merge(child).await })
    }

    fn discard(&mut self, branch: BranchId) -> Result<(), StoreError> {
        self.call(move |backend| async move { backend.discard(branch).await })
    }

    fn commit(&mut self, root: BranchId) -> Result<CommitId, StoreError> {
        self.call(move |backend| async move { backend.commit(root, None).await })
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        self.call(move |backend| async move { backend.branch_info(branch).await })
    }

    fn create(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
        _budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError> {
        self.call(move |backend| async move { backend.create(branch, key, cells).await })
            .map(with_free_receipt)
    }

    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        let projection = projection.map(<[CellName]>::to_vec);
        self.call(move |backend| async move {
            let found = backend.read(target, key).await?;
            Ok(found.map(|record| record.to_record(key, projection.as_deref())))
        })
        .map(with_free_receipt)
    }

    fn patch(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        _budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError> {
        self.call(move |backend| async move {
            backend.patch(branch, key, expected_version, changes).await
        })
        .map(with_free_receipt)
    }

    fn delete(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        _budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        self.call(move |backend| async move { backend.delete(branch, key, expected_version).await })
            .map(with_free_receipt)
    }

    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        _budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        let at = at.unwrap_or_else(|| self.head());
        let query = query.clone();
        self.call(move |backend| async move {
            let state = backend.state_at(at).await?;

            // Matching is `Filter::matches`, shared with every other
            // implementation, so filter semantics cannot diverge by backend.
            let mut matched = Vec::new();
            for (key, record) in &state {
                let full = record.to_record(*key, None);
                if query.filter.matches(&full)? {
                    matched.push(full);
                }
            }

            if let Some(sort) = &query.sort {
                sort_records(&mut matched, sort);
            }

            let total_matched = query.total_matched.then_some(matched.len() as u64);
            let records = matched
                .into_iter()
                .skip(query.page.offset as usize)
                .take(query.page.limit as usize)
                .map(|record| project(record, query.projection.as_deref()))
                .collect();

            Ok(QueryResult {
                records,
                total_matched,
            })
        })
        .map(with_free_receipt)
    }

    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        _budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        let at = at.unwrap_or_else(|| self.head());
        let filter = filter.clone();
        self.call(move |backend| async move {
            let state = backend.state_at(at).await?;
            let mut matched = 0;
            for (key, record) in &state {
                if filter.matches(&record.to_record(*key, None))? {
                    matched += 1;
                }
            }
            Ok(matched)
        })
        .map(with_free_receipt)
    }

    fn branch_digest(&self, branch: BranchId) -> Result<[u8; 32], StoreError> {
        self.call(move |backend| async move { backend.branch_digest(branch).await })
    }
}

impl StoreExt for ValkeyStore {
    fn commit_tagged(&mut self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        self.call(move |backend| async move { backend.commit(root, Some(tag)).await })
    }

    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        self.call(move |backend| async move { backend.commit_by_tag(tag).await })
    }

    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        self.call(move |backend| async move { backend.changes(commit).await })
    }

    fn apply(
        &mut self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        // All-or-nothing: keep the pre-batch diff and put it back untouched if
        // any op fails. Batching must be a transport optimization, never a
        // semantic one.
        let (saved, version) =
            self.call(move |backend| async move { backend.snapshot_diff(branch).await })?;

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
                    .map(|new| WriteOutcome::Patched(new.into_value())),
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
                    self.call(move |backend| async move {
                        backend.restore_diff(branch, saved, version).await
                    })?;
                    return Err(error);
                }
            }
        }

        // One batch, one version bump — not one per op, which is what the
        // per-call increments above would otherwise leave behind.
        let bumped = BranchVersion(version.0 + 1);
        self.call(move |backend| async move { backend.set_version(branch, bumped).await })?;
        Ok(with_free_receipt(outcomes))
    }

    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        self.call(move |backend| async move { backend.commit_hash(commit).await })
    }

    fn retention(&self) -> (CommitId, CommitId) {
        // Nothing is pruned yet, so the window is the whole history.
        (CommitId::GENESIS, self.head())
    }

    fn get_many(
        &self,
        target: ReadTarget,
        keys: &[RecordKey],
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError> {
        let keys = keys.to_vec();
        let projection = projection.map(<[CellName]>::to_vec);
        self.call(move |backend| async move {
            let mut found = Vec::with_capacity(keys.len());
            for key in keys {
                found.push(
                    backend
                        .read(target, key)
                        .await?
                        .map(|record| record.to_record(key, projection.as_deref())),
                );
            }
            Ok(found)
        })
        .map(with_free_receipt)
    }
}

/// Drop the cells a projection excludes. Applied after matching, because a
/// predicate may name a cell the caller did not ask to see.
fn project(mut record: Record, projection: Option<&[CellName]>) -> Record {
    if let Some(wanted) = projection {
        record.cells.retain(|(name, _)| wanted.contains(name));
    }
    record
}

/// Sort matched records by one cell, in the requested direction.
///
/// Records lacking the sort cell sort last, and ascending key order is the
/// tie-break — so the ordering is total and two implementations agreeing on
/// the match set also agree on the page.
fn sort_records(records: &mut [Record], sort: &Sort) {
    records.sort_by(|left, right| {
        let ordering = match (left.sort_key(&sort.cell), right.sort_key(&sort.cell)) {
            (Some(left_value), Some(right_value)) => left_value.cmp(&right_value),
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (None, None) => core::cmp::Ordering::Equal,
        };
        let ordering = ordering.then_with(|| left.key.cmp(&right.key));
        match sort.direction {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });
}
