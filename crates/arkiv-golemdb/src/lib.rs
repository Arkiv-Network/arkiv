//! [`GolemDbStore`]: Arkiv's [`Store`] over GolemDB, in-process.
//!
//! It translates; what GolemDB lacks is [`StoreError::NotImplemented`], with
//! two placeholders: record versions are always 0 and `branch_hash` is a
//! constant. Once another branch commits, an older branch is dead: the adapter
//! reports it and never retries or rebases it.
//!
//! Reads are at the head only, as GolemDB keeps no history yet: a read or query
//! at an older commit is answered from the head. Segments are GolemDB immutable
//! data, and commit tags a segment of the adapter's own (see [`segments`]).
//! Queries run on GolemDB's index except negated predicates, which are applied
//! to the matches afterwards; paging and sorting are done here, as GolemDB
//! does not page yet.
//!
//! A commit is durable once it returns; a branch not yet committed is lost
//! when the store closes or the process dies. Dropping the last clone closes
//! the database.
//!
//! `head` panics if it cannot read the head. A full disk or database, and any
//! other I/O failure, is `Internal`, which callers must treat as fatal. A stale
//! branch or commit is `HandleInvalid`.

mod convert;
mod query;
mod segments;

use std::path::Path;
use std::sync::{Arc, Mutex};

use arkiv_interfaces::store::{
    BranchId, BranchInfo, BranchVersion, Budget, Cell, CellChange, CellName, CommitId, Filter,
    Metered, Origin, Query, QueryResult, ReadTarget, Record, RecordChange, RecordKey,
    RecordVersion, SealedCommit, SegmentAddress, SegmentKey, SegmentOrdinal, SegmentRow,
    SortDirection, Store, StoreError, StoreExt, WriteOp, WriteOutcome,
};
use golemdb_api as golemdb;
use golemdb_api::{Api, ApiError, ImmutableDataAddress, ImmutableDataReadOptions};

pub use golemdb_api::{CellLimits, HashAlgorithm, OpenError};

use convert::{
    arkiv_record, branch_id, commit_id, golemdb_cell, golemdb_commit, metered, record_key,
    store_error,
};

/// Fixed when the database is created; reopening with anything else fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GolemDbConfig {
    pub hash_function: HashAlgorithm,
    pub cell_limits: CellLimits,
}

impl GolemDbConfig {
    /// What an Arkiv node runs on: Keccak, and cells as large as Arkiv's own
    /// limits allow (`arkiv_interfaces::constants`), with room for the `$`
    /// prefixes on Arkiv's cell names.
    pub const fn arkiv() -> Self {
        Self {
            hash_function: HashAlgorithm::Keccak256,
            cell_limits: CellLimits {
                max_cell_name_len: 64,
                max_str_len: 128,
                max_bytes_len: 128 * 1024,
            },
        }
    }
}

/// The scheme a store URL names a GolemDB directory with: `golemdb:///data/x`.
pub const URL_SCHEME: &str = "golemdb://";

#[derive(Clone)]
pub struct GolemDbStore {
    database: golemdb::Database,
    /// The last match set, for the commit and filter it was asked for.
    ///
    /// GolemDB does not page, so every page of a query is the whole match set
    /// sliced here; a client walking a large result page by page would
    /// otherwise pay for the whole set once per page. A client pages at a
    /// fixed commit, which falls behind the head as blocks come: keyed by the
    /// head, the set would be recomputed for nearly every page of a large
    /// result. Keyed by the commit asked for, an older commit is answered
    /// from the head as it was when first asked, which is no further from
    /// that commit than the head is now.
    last_match: Arc<Mutex<Option<LastMatch>>>,
}

/// A match set: the commit and filter it was asked for, and the keys.
type LastMatch = (CommitId, Filter, Arc<Vec<RecordKey>>);

impl core::fmt::Debug for GolemDbStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GolemDbStore").finish_non_exhaustive()
    }
}

impl GolemDbStore {
    /// Open `directory`, creating it with `config` if it is new. Only one
    /// process may open a directory.
    pub fn open(directory: &Path, config: &GolemDbConfig) -> Result<Self, OpenError> {
        let genesis = golemdb::Genesis::new(
            config.hash_function,
            config.cell_limits,
            Some(segments::declaration()),
        );
        let database = golemdb::Database::open(directory, &golemdb::OpenConfig::new(genesis))?;
        Ok(Self {
            database,
            last_match: Arc::default(),
        })
    }

    /// Open the directory a `golemdb://` URL names, with [`GolemDbConfig::arkiv`].
    /// `None` if `url` is not a GolemDB URL.
    pub fn open_url(url: &str) -> Option<Result<Self, OpenError>> {
        let directory = url.strip_prefix(URL_SCHEME)?;
        Some(Self::open(Path::new(directory), &GolemDbConfig::arkiv()))
    }

    /// GolemDB answers only the head, so a commit-targeted read that is not at
    /// the head falls back to it rather than failing.
    fn read_target(&self, target: ReadTarget) -> golemdb::ReadTarget {
        match target {
            ReadTarget::Branch(branch) => golemdb::ReadTarget::Branch(branch_id(branch)),
            ReadTarget::Commit(commit) if commit == self.head() => {
                golemdb::ReadTarget::Commit(golemdb_commit(commit))
            }
            ReadTarget::Commit(_) => golemdb::ReadTarget::Head,
        }
    }

    /// The keys of every committed record matching `filter`, ascending, for
    /// a query at `at` (answered from the head).
    fn matching(&self, at: CommitId, filter: &Filter) -> Result<Arc<Vec<RecordKey>>, StoreError> {
        if let Some((cached_at, cached, keys)) = &*self.last_match.lock().expect("not poisoned")
            && *cached_at == at
            && cached == filter
        {
            return Ok(Arc::clone(keys));
        }
        let keys = Arc::new(self.evaluate(filter)?);
        *self.last_match.lock().expect("not poisoned") =
            Some((at, filter.clone(), Arc::clone(&keys)));
        Ok(keys)
    }

    fn evaluate(&self, filter: &Filter) -> Result<Vec<RecordKey>, StoreError> {
        let lowered = query::lower(filter)?;
        let operation = golemdb::CountOp::filter(lowered.filter).budget(golemdb::Budget::Unlimited);
        let mut keys: Vec<_> = self
            .database
            .query_keys(None, operation)
            .map_err(store_error)?
            .into_iter()
            .map(|key| RecordKey(key.0))
            .collect();
        if lowered.negations {
            let mut kept = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(record) = self.read(ReadTarget::Commit(self.head()), key, None)?
                    && filter.matches(&record)?
                {
                    kept.push(key);
                }
            }
            keys = kept;
        }
        Ok(keys)
    }

    fn read(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
    ) -> Result<Option<Record>, StoreError> {
        Ok(self.get(target, key, projection, None)?.into_value())
    }

    /// The tags row a commit wrote, if it was tagged.
    fn tag_row(&self, commit: CommitId) -> Result<Option<golemdb::ImmutableDataRow>, StoreError> {
        let rows = self
            .database
            .immutable_data_rows_of(
                segments::COMMIT_TAGS,
                golemdb_commit(commit),
                &ImmutableDataReadOptions::all(),
            )
            .map_err(store_error)?;
        Ok(rows.into_iter().next())
    }
}

impl Store for GolemDbStore {
    /// Panics if the head cannot be read: an I/O failure, where stopping is
    /// the only safe answer.
    fn head(&self) -> CommitId {
        commit_id(
            self.database
                .head()
                .expect("GolemDB could not read its head"),
        )
    }

    /// GolemDB branches only over the head, so `at` must be the head.
    fn begin(&self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        let branch = self.database.begin().map_err(store_error)?;
        if let Some(at) = at {
            let origin = self.database.branch_info(branch).map_err(store_error)?;
            if commit_id(origin.commit_id) != at {
                let _ = self.database.discard(branch);
                return Err(StoreError::HandleInvalid);
            }
        }
        Ok(BranchId(branch.get()))
    }

    fn checkpoint(&self, branch: BranchId) -> Result<(), StoreError> {
        self.database
            .checkpoint(branch_id(branch))
            .map_err(store_error)
    }

    fn rollback(&self, branch: BranchId) -> Result<(), StoreError> {
        self.database
            .rollback(branch_id(branch))
            .map_err(store_error)
    }

    fn seal(&self, branch: BranchId) -> Result<SealedCommit, StoreError> {
        let sealed = self.database.seal(branch_id(branch)).map_err(store_error)?;
        Ok(SealedCommit {
            commit_nr: commit_id(sealed.commit_id),
            state_root: sealed.state_root,
            index_root: sealed.index_root,
        })
    }

    fn discard(&self, branch: BranchId) -> Result<(), StoreError> {
        self.database
            .discard(branch_id(branch))
            .map_err(store_error)
    }

    fn commit(&self, root: BranchId) -> Result<CommitId, StoreError> {
        self.database
            .commit(branch_id(root))
            .map(commit_id)
            .map_err(store_error)
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        let info = self
            .database
            .branch_info(branch_id(branch))
            .map_err(store_error)?;
        Ok(BranchInfo {
            origin: Origin::Commit(commit_id(info.commit_id)),
            version: BranchVersion(info.version),
            depth: 0,
        })
    }

    fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
        _budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError> {
        let mut operation = golemdb::RecordOp::create(record_key(key));
        for (name, cell) in &cells {
            operation = operation
                .insert(name, golemdb_cell(cell)?)
                .map_err(store_error)?;
        }
        self.database
            .create(branch_id(branch), operation)
            .map(|key| metered(RecordKey(key.0)))
            .map_err(store_error)
    }

    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        let operation = || {
            let operation = golemdb::RecordOp::get(record_key(key));
            match projection {
                Some(names) => operation.only(names.iter().map(String::as_bytes)),
                None => operation,
            }
        };
        let read = match self.database.get(self.read_target(target), operation()) {
            // The head moved between choosing the target and reading it.
            Err(ApiError::CommitUnavailable { .. }) if matches!(target, ReadTarget::Commit(_)) => {
                self.database.get(golemdb::ReadTarget::Head, operation())
            }
            read => read,
        };
        match read {
            Ok(record) => Ok(metered(Some(arkiv_record(&record)?))),
            Err(ApiError::NotFound) => Ok(metered(None)),
            Err(error) => Err(store_error(error)),
        }
    }

    /// GolemDB has no record versions, so a version guard cannot be honoured.
    fn patch(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        _budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError> {
        if expected_version.is_some() {
            return Err(StoreError::NotImplemented);
        }
        let mut operation = golemdb::RecordOp::patch(record_key(key));
        for (name, change) in &changes {
            operation = match change {
                CellChange::Set(cell) => operation.set(name, golemdb_cell(cell)?),
                CellChange::Remove => operation.remove(name),
            }
            .map_err(store_error)?;
        }
        self.database
            .patch(branch_id(branch), operation)
            .map(|()| metered(RecordVersion(0)))
            .map_err(store_error)
    }

    fn delete(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        _budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        if expected_version.is_some() {
            return Err(StoreError::NotImplemented);
        }
        self.database
            .delete(
                branch_id(branch),
                golemdb::RecordOp::delete(record_key(key)),
            )
            .map(metered)
            .map_err(store_error)
    }

    /// Matched on GolemDB's index at the head; sorted, paged and projected
    /// here.
    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        _budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        let head = self.head();
        let matched = self.matching(at.unwrap_or(head), &query.filter)?;
        let at = ReadTarget::Commit(head);
        let mut sorted_keys = None;
        if let Some(sort) = &query.sort {
            // As the reference store sorts: records lacking the cell last,
            // ascending key as the tie-break.
            let mut sorted = Vec::with_capacity(matched.len());
            for &key in matched.iter() {
                let value = self
                    .read(at, key, Some(core::slice::from_ref(&sort.cell)))?
                    .and_then(|record| record.sort_key(&sort.cell));
                sorted.push((value, key));
            }
            sorted.sort_by(|(left, left_key), (right, right_key)| {
                let ordering = match (left, right) {
                    (Some(left), Some(right)) => match sort.direction {
                        SortDirection::Ascending => left.cmp(right),
                        SortDirection::Descending => right.cmp(left),
                    },
                    (Some(_), None) => core::cmp::Ordering::Less,
                    (None, Some(_)) => core::cmp::Ordering::Greater,
                    (None, None) => core::cmp::Ordering::Equal,
                };
                ordering.then(left_key.cmp(right_key))
            });
            sorted_keys = Some(sorted.into_iter().map(|(_, key)| key).collect::<Vec<_>>());
        }
        let keys = sorted_keys.as_deref().unwrap_or(&matched);
        let total_matched = query.total_matched.then_some(keys.len() as u64);
        let mut records = Vec::new();
        for &key in keys
            .iter()
            .skip(query.page.offset as usize)
            .take(query.page.limit as usize)
        {
            if let Some(record) = self.read(at, key, query.projection.as_deref())? {
                records.push(record);
            }
        }
        Ok(metered(QueryResult {
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
        let lowered = query::lower(filter)?;
        if lowered.negations {
            let at = at.unwrap_or_else(|| self.head());
            return Ok(metered(self.matching(at, filter)?.len() as u64));
        }
        let operation = golemdb::CountOp::filter(lowered.filter).budget(golemdb::Budget::Unlimited);
        self.database
            .count(None, operation)
            .map(metered)
            .map_err(store_error)
    }

    /// A constant. The one production caller discards it, and GolemDB has no
    /// digest of an unsealed branch.
    fn branch_hash(&self, _branch: BranchId) -> Result<[u8; 32], StoreError> {
        Ok([0; 32])
    }

    fn segment_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<SegmentKey>,
        row: SegmentRow,
    ) -> Result<SegmentOrdinal, StoreError> {
        if !segments::is_arkiv(segment) {
            return Err(StoreError::InvalidArgument);
        }
        self.database
            .immutable_data_append(
                branch_id(branch),
                segment,
                key.map(segments::key),
                segments::golemdb_row(row),
            )
            .map_err(store_error)
    }

    fn segment_get(
        &self,
        segment: &str,
        address: SegmentAddress,
    ) -> Result<Option<SegmentRow>, StoreError> {
        let address = match address {
            SegmentAddress::Ordinal(ordinal) => ImmutableDataAddress::Ordinal(ordinal),
            SegmentAddress::Key(key) => ImmutableDataAddress::Key(segments::key(key)),
        };
        match self
            .database
            .immutable_data_get(segment, address, &ImmutableDataReadOptions::all())
        {
            Ok(row) => Ok(Some(segments::arkiv_row(&row))),
            Err(ApiError::NotFound) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn segment_ordinal_of(
        &self,
        segment: &str,
        key: SegmentKey,
    ) -> Result<Option<SegmentOrdinal>, StoreError> {
        match self
            .database
            .immutable_data_ordinal_of(segment, segments::key(key))
        {
            Err(ApiError::NotFound) => Ok(None),
            found => found.map_err(store_error),
        }
    }

    fn segment_range_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<core::ops::Range<SegmentOrdinal>, StoreError> {
        match self
            .database
            .immutable_data_range_of(segment, golemdb_commit(commit))
        {
            // A segment the store has never seen has no length to report.
            Err(ApiError::NotFound) => Ok(0..0),
            range => range.map_err(store_error),
        }
    }

    fn segment_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<Vec<SegmentRow>, StoreError> {
        match self.database.immutable_data_rows_of(
            segment,
            golemdb_commit(commit),
            &ImmutableDataReadOptions::all(),
        ) {
            Ok(rows) => Ok(rows.iter().map(segments::arkiv_row).collect()),
            Err(ApiError::NotFound) => Ok(Vec::new()),
            Err(error) => Err(store_error(error)),
        }
    }
}

impl StoreExt for GolemDbStore {
    /// The tag, the commit number and the sealed root go into the tags
    /// segment on the branch itself, so they publish atomically with it.
    fn commit_tagged(&self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        let sealed = self.seal(root)?;
        self.database
            .immutable_data_append(
                branch_id(root),
                segments::COMMIT_TAGS,
                Some(golemdb::ImmutableDataKey(tag)),
                segments::tag_row(sealed.commit_nr.0, tag, sealed.state_root),
            )
            .map_err(store_error)?;
        let committed = self.commit(root)?;
        if committed != sealed.commit_nr {
            return Err(StoreError::Internal);
        }
        Ok(committed)
    }

    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        match self.database.immutable_data_get(
            segments::COMMIT_TAGS,
            ImmutableDataAddress::Key(golemdb::ImmutableDataKey(tag)),
            &ImmutableDataReadOptions::columns([segments::TAG_COMMIT]),
        ) {
            Ok(row) => Ok(Some(CommitId(segments::tag_column(
                &row,
                segments::TAG_COMMIT,
            )?))),
            Err(ApiError::NotFound) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn tag_of(&self, commit: CommitId) -> Result<Option<[u8; 32]>, StoreError> {
        self.tag_row(commit)?
            .map(|row| segments::tag_column(&row, segments::TAG_TAG))
            .transpose()
    }

    /// GolemDB keeps no changesets.
    fn changes(&self, _commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        Err(StoreError::NotImplemented)
    }

    /// All-or-nothing through a checkpoint: a failed write rolls the batch
    /// back. So it needs an unsealed branch, as every write does.
    fn apply(
        &self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        self.checkpoint(branch)?;
        let mut outcomes = Vec::with_capacity(ops.len());
        for op in ops {
            let outcome = match op {
                WriteOp::Create { key, cells } => self
                    .create(branch, key, cells, None)
                    .map(|_| WriteOutcome::Created),
                WriteOp::Patch {
                    key,
                    expected_version,
                    changes,
                } => self
                    .patch(branch, key, expected_version, changes, None)
                    .map(|version| WriteOutcome::Patched(version.into_value())),
                WriteOp::Delete {
                    key,
                    expected_version,
                } => self
                    .delete(branch, key, expected_version, None)
                    .map(|_| WriteOutcome::Deleted),
            };
            match outcome {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => {
                    self.rollback(branch)?;
                    return Err(error);
                }
            }
        }
        Ok(metered(outcomes))
    }

    /// The root a tagged commit sealed to. An untagged commit kept none.
    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        self.tag_row(commit)?
            .map(|row| segments::tag_column(&row, segments::TAG_STATE_ROOT))
            .transpose()?
            .ok_or(StoreError::NotFound)
    }

    /// Only the head is readable as itself; see the crate docs.
    fn retention(&self) -> (CommitId, CommitId) {
        let head = self.head();
        (head, head)
    }

    fn get_many(
        &self,
        target: ReadTarget,
        keys: &[RecordKey],
        projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError> {
        keys.iter()
            .map(|key| self.read(target, *key, projection))
            .collect::<Result<_, _>>()
            .map(metered)
    }
}

#[cfg(test)]
mod tests;
