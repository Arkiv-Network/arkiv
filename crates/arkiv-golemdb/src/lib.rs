//! [`GolemDbStore`]: Arkiv's [`Store`] over GolemDB, in-process.
//!
//! It translates; what GolemDB lacks is [`StoreError::NotImplemented`], with
//! two placeholders: record versions are always 0 and `branch_hash` is a
//! constant. Once another branch commits, an older branch is dead: the adapter
//! reports it and never retries or rebases it. Reads are at the head only, as
//! GolemDB keeps no history yet. Segments, the append-only rows of blocks,
//! transactions and receipts, are GolemDB immutable data; so are commit tags,
//! in a segment of the adapter's own, `commit_tags`. A row's ordinal by its
//! key, and a commit's range of ordinals, are not implemented yet.
//!
//! A commit is durable once it returns; a branch not yet committed is lost
//! when the store closes or the process dies. Dropping the last clone closes
//! the database.
//!
//! `head` panics if it cannot read the head. A full disk or database, and any
//! other I/O failure, is `Internal`, which callers must treat as fatal. A stale
//! branch or commit is `HandleInvalid`.

mod convert;
mod tags;

use std::path::Path;

use arkiv_interfaces::store::{
    BranchId, BranchInfo, BranchVersion, Budget, Cell, CellChange, CellKind, CellName, CommitId,
    Filter, Metered, Origin, Query, QueryResult, ReadTarget, Record, RecordChange, RecordKey,
    RecordVersion, SealedCommit, SegmentAddress, SegmentKey, SegmentOrdinal, SegmentRow, Store,
    StoreError, StoreExt, WriteOp, WriteOutcome,
};
use golemdb_api as golemdb;
use golemdb_api::{Api, ApiError};

pub use golemdb_api::{
    CellLimits, Genesis, HashAlgorithm, OpenError, PricingConfig, PruningConfig, RecordLimits,
};

use convert::{
    arkiv_query_result, arkiv_record, branch_id, commit_id, count_op, golemdb_budget, golemdb_cell,
    golemdb_commit, immutable_data_address, immutable_data_key, immutable_data_row, metered,
    metered_result, query_op, record_key, segment_row, store_error,
};
use tags::TagRow;

/// Fixed when the database is created; reopening with anything else fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GolemDbConfig {
    /// GolemDB's settings, as the chain's genesis file gives them. They declare
    /// no segments; those are [`segments`](Self::segments).
    pub genesis: Genesis,
    /// The segments rows may be appended to.
    pub segments: Vec<SegmentSpec>,
}

/// A segment: its name, and how many byte columns each of its rows has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSpec {
    pub name: String,
    pub columns: usize,
}

/// Commits per segment file, the span GolemDB's design uses.
const SHARD_SPAN: u64 = 500_000;

impl GolemDbConfig {
    /// [`genesis`](Self::genesis) with the host's segments, as LZ4-compressed
    /// byte columns, and the adapter's own.
    fn full_genesis(&self) -> Result<Genesis, OpenError> {
        if self.genesis.immutable_data.is_some() {
            return Err(OpenError::InvalidConfig(
                "the genesis sets `immutable_data`, but the node declares its own segments".into(),
            ));
        }
        let segments = self
            .segments
            .iter()
            .map(|spec| golemdb::ImmutableDataSegment {
                name: spec.name.clone(),
                columns: (0..spec.columns)
                    .map(|column| golemdb::ImmutableDataColumn::new(format!("c{column}")))
                    .collect(),
                compression: golemdb::ImmutableDataCompression::Lz4,
            })
            .chain([tags::segment()])
            .collect();
        let mut genesis = self.genesis.clone();
        genesis.immutable_data = Some(golemdb::ImmutableDataConfig {
            shard_span: SHARD_SPAN,
            segments,
        });
        Ok(genesis)
    }
}

#[derive(Clone)]
pub struct GolemDbStore {
    database: golemdb::Database,
}

impl core::fmt::Debug for GolemDbStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GolemDbStore").finish_non_exhaustive()
    }
}

impl GolemDbStore {
    /// Open `directory`, creating it with `config` if it is new. Only one
    /// process may open a directory. `pruning` is how many past commits to
    /// keep, `None` all; it may change between opens.
    pub fn open(
        directory: &Path,
        config: &GolemDbConfig,
        pruning: Option<PruningConfig>,
    ) -> Result<Self, OpenError> {
        let mut open = golemdb::OpenConfig::new(config.full_genesis()?);
        if let Some(pruning) = pruning {
            open = open.with_pruning(pruning);
        }
        let database = golemdb::Database::open(directory, &open)?;
        Ok(Self { database })
    }

    /// A new database in memory, created with `config`; gone when the last
    /// clone drops. It computes the same roots as one on disk.
    pub fn open_memory(config: &GolemDbConfig) -> Result<Self, OpenError> {
        let database = golemdb::Database::open_memory(&config.full_genesis()?)?;
        Ok(Self { database })
    }

    /// Seal `branch`, stage its commit's `commit_tags` row and commit. If the
    /// commit fails, the row comes off again, so a retry stages only one.
    fn commit_with_tag(
        &self,
        branch: BranchId,
        tag: Option<[u8; 32]>,
    ) -> Result<CommitId, StoreError> {
        let branch = branch_id(branch);
        let sealed = self.database.seal(branch).map_err(store_error)?;
        let row = TagRow {
            commit: sealed.commit_id.get(),
            tag,
            state_root: sealed.state_root,
        };
        let ordinal = self
            .database
            .immutable_data_append(
                branch,
                tags::SEGMENT,
                tag.map(|tag| immutable_data_key(SegmentKey(tag))),
                row.to_row(),
            )
            .map_err(store_error)?;
        self.database
            .commit(branch)
            .map(commit_id)
            .map_err(|error| {
                // A losing branch is gone already, so this may fail; that is fine.
                let _ = self
                    .database
                    .immutable_data_truncate(branch, tags::SEGMENT, ordinal);
                store_error(error)
            })
    }

    /// The `commit_tags` row of `commit`: exactly one once it is committed,
    /// none for the empty genesis or a commit yet to come.
    fn tag_row(&self, commit: CommitId) -> Result<Option<TagRow>, StoreError> {
        let rows = self.rows_of(
            tags::SEGMENT,
            commit,
            &golemdb::ImmutableDataReadOptions::all(),
        )?;
        match rows.as_deref() {
            None | Some([]) => Ok(None),
            Some([row]) => TagRow::from_row(row).map(Some),
            Some(_) => Err(StoreError::Internal),
        }
    }

    /// The rows `commit` appended to `segment`, or `None` for a commit yet to
    /// come.
    fn rows_of(
        &self,
        segment: &str,
        commit: CommitId,
        options: &golemdb::ImmutableDataReadOptions,
    ) -> Result<Option<Vec<golemdb::ImmutableDataRow>>, StoreError> {
        match self
            .database
            .immutable_data_rows_of(segment, golemdb_commit(commit), options)
        {
            Ok(rows) => Ok(Some(rows)),
            Err(ApiError::CommitUnavailable { .. }) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    /// Arkiv appends only after seal; GolemDB itself would take the row
    /// sooner.
    fn ensure_sealed(&self, branch: BranchId) -> Result<(), StoreError> {
        let info = self
            .database
            .branch_info(branch_id(branch))
            .map_err(store_error)?;
        if info.sealed {
            Ok(())
        } else {
            Err(StoreError::NotSealed)
        }
    }

    /// `commit_tags` is the adapter's own: a host row there would break its
    /// one row per commit.
    fn append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<SegmentKey>,
        row: &SegmentRow,
    ) -> Result<SegmentOrdinal, StoreError> {
        if segment == tags::SEGMENT {
            return Err(StoreError::InvalidArgument);
        }
        self.database
            .immutable_data_append(
                branch_id(branch),
                segment,
                key.map(immutable_data_key),
                immutable_data_row(row),
            )
            .map_err(store_error)
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
        self.commit_with_tag(root, None)
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
        budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError> {
        let mut operation = golemdb::RecordOp::create(record_key(key), golemdb_budget(budget));
        for (name, cell) in &cells {
            let value = golemdb_cell(cell)?;
            operation = match cell.kind {
                CellKind::Attribute => operation.attribute(name, value),
                CellKind::Field => operation.field(name, value),
            };
        }
        metered_result(self.database.create(branch_id(branch), operation))
            .map(|key| metered(RecordKey(key.0)))
    }

    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        // GolemDB's own records sit at fixed keys a caller can name. None is
        // Arkiv's, and `#recordKeys` holds a cell for every record there is.
        if golemdb_cells::system::by_key(&key.0).is_some() {
            return Ok(metered(None));
        }
        let target = match target {
            ReadTarget::Branch(branch) => golemdb::ReadTarget::Branch(branch_id(branch)),
            ReadTarget::Commit(commit) => golemdb::ReadTarget::Commit(golemdb_commit(commit)),
        };
        let mut operation = golemdb::RecordOp::get(record_key(key), golemdb_budget(budget));
        if let Some(names) = projection {
            operation = operation.only(names.iter().map(String::as_bytes));
        }
        match metered_result(self.database.get(target, operation)) {
            Ok(record) => Ok(metered(Some(arkiv_record(&record)?))),
            Err(StoreError::NotFound) => Ok(metered(None)),
            Err(error) => Err(error),
        }
    }

    /// GolemDB has no record versions, so a version guard cannot be honoured.
    fn patch(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError> {
        if expected_version.is_some() {
            return Err(StoreError::NotImplemented);
        }
        let mut operation = golemdb::RecordOp::patch(record_key(key), golemdb_budget(budget));
        for (name, change) in &changes {
            operation = match change {
                // A write, as in `create`, not GolemDB's `set`: `set` only
                // replaces a cell the record already holds, and keeps its
                // stored kind. Arkiv's `Set` adds a missing cell, which the
                // account and entity paths rely on, and gives it the kind it
                // names, which is how an attribute becomes indexed.
                CellChange::Set(cell) => {
                    let value = golemdb_cell(cell)?;
                    match cell.kind {
                        CellKind::Attribute => operation.attribute(name, value),
                        CellKind::Field => operation.field(name, value),
                    }
                }
                CellChange::Remove => operation.remove(name),
            };
        }
        metered_result(self.database.patch(branch_id(branch), operation))
            .map(|()| metered(RecordVersion(0)))
    }

    fn delete(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        if expected_version.is_some() {
            return Err(StoreError::NotImplemented);
        }
        metered_result(self.database.delete(
            branch_id(branch),
            golemdb::RecordOp::delete(record_key(key), golemdb_budget(budget)),
        ))
        .map(metered)
    }

    /// The query runs as written, so what GolemDB cannot run yet, a `Not`, is
    /// `NotImplemented`. Matches come in the sort's order, or with no sort in
    /// GolemDB's own order, the order the records were created in, which also
    /// orders the ties of a sort. Paging runs over that order within GolemDB's
    /// own query limit, over which a page is `LimitExceeded`.
    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        let page = metered_result(
            self.database
                .query(at.map(golemdb_commit), query_op(query, budget)?),
        )?;
        arkiv_query_result(&page).map(metered)
    }

    /// A count does not page, so it runs whenever GolemDB can express the
    /// filter.
    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        metered_result(
            self.database
                .count(at.map(golemdb_commit), count_op(filter, budget)?),
        )
        .map(metered)
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
        self.ensure_sealed(branch)?;
        self.append(branch, segment, key, &row)
    }

    /// One sealed check for the batch, and no row copied first.
    fn segment_append_many(
        &self,
        branch: BranchId,
        writes: &[(&str, Option<SegmentKey>, SegmentRow)],
    ) -> Result<Vec<SegmentOrdinal>, StoreError> {
        self.ensure_sealed(branch)?;
        writes
            .iter()
            .map(|(segment, key, row)| self.append(branch, segment, *key, row))
            .collect()
    }

    fn segment_get(
        &self,
        segment: &str,
        address: SegmentAddress,
    ) -> Result<Option<SegmentRow>, StoreError> {
        let read = self.database.immutable_data_get(
            segment,
            immutable_data_address(address),
            &golemdb::ImmutableDataReadOptions::all(),
        );
        match read {
            Ok(row) => Ok(Some(segment_row(&row))),
            Err(ApiError::NotFound) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn segment_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<Vec<SegmentRow>, StoreError> {
        let rows = self
            .rows_of(segment, commit, &golemdb::ImmutableDataReadOptions::all())?
            .ok_or(StoreError::NotFound)?;
        Ok(rows.iter().map(segment_row).collect())
    }
}

/// Tags and roots stay for every commit, though state reads at the head only:
/// the retention window is the head alone, so a tag found for an older commit
/// does not make its state readable. GolemDB keeps no changesets.
impl StoreExt for GolemDbStore {
    fn commit_tagged(&self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        self.commit_with_tag(root, Some(tag))
    }

    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        let read = self.database.immutable_data_get(
            tags::SEGMENT,
            golemdb::ImmutableDataAddress::Key(immutable_data_key(SegmentKey(tag))),
            &tags::commit_only(),
        );
        match read {
            Ok(row) => Ok(Some(CommitId(tags::commit(&row)?))),
            Err(ApiError::NotFound) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn tag_of(&self, commit: CommitId) -> Result<Option<[u8; 32]>, StoreError> {
        Ok(self.tag_row(commit)?.and_then(|row| row.tag))
    }

    fn changes(&self, _commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        Err(StoreError::NotImplemented)
    }

    /// All or nothing would need a checkpoint, and a successful batch would
    /// leave that frame open on the caller's branch; GolemDB cannot fold it
    /// back.
    fn apply(
        &self,
        _branch: BranchId,
        _ops: Vec<WriteOp>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        Err(StoreError::NotImplemented)
    }

    /// The state root `commit` sealed.
    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        self.tag_row(commit)?
            .map(|row| row.state_root)
            .ok_or(StoreError::NotFound)
    }

    fn retention(&self) -> (CommitId, CommitId) {
        let head = self.head();
        (head, head)
    }

    fn get_many(
        &self,
        _target: ReadTarget,
        _keys: &[RecordKey],
        _projection: Option<&[CellName]>,
        _budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError> {
        Err(StoreError::NotImplemented)
    }
}

#[cfg(test)]
mod tests;
