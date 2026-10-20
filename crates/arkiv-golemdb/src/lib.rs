//! [`GolemDbStore`]: Arkiv's [`Store`] over GolemDB, in-process.
//!
//! It translates; what GolemDB lacks is [`StoreError::NotImplemented`], with
//! two placeholders: record versions are always 0 and `branch_hash` is a
//! constant. Once another branch commits, an older branch is dead: the adapter
//! reports it and never retries or rebases it. Reads are at the head only, as
//! GolemDB keeps no history yet. Segments (blocks, transactions, receipts) and
//! the `StoreExt` host extensions are not implemented yet.
//!
//! A commit is durable once it returns; a branch not yet committed is lost
//! when the store closes or the process dies. Dropping the last clone closes
//! the database.
//!
//! `head` panics if it cannot read the head. A full disk or database, and any
//! other I/O failure, is `Internal`, which callers must treat as fatal. A stale
//! branch or commit is `HandleInvalid`.

mod convert;

use std::path::Path;

use arkiv_interfaces::store::{
    BranchId, BranchInfo, BranchVersion, Budget, Cell, CellChange, CellName, CommitId, Filter,
    Metered, Origin, Query, QueryResult, ReadTarget, Record, RecordKey, RecordVersion,
    SealedCommit, Store, StoreError,
};
use golemdb_api as golemdb;
use golemdb_api::{Api, ApiError};

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
    /// process may open a directory.
    pub fn open(directory: &Path, config: &GolemDbConfig) -> Result<Self, OpenError> {
        let genesis = golemdb::Genesis::new(config.hash_function, config.cell_limits);
        let database = golemdb::Database::open(directory, &golemdb::OpenConfig::new(genesis))?;
        Ok(Self { database })
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
        let target = match target {
            ReadTarget::Branch(branch) => golemdb::ReadTarget::Branch(branch_id(branch)),
            ReadTarget::Commit(commit) => golemdb::ReadTarget::Commit(golemdb_commit(commit)),
        };
        let mut operation = golemdb::RecordOp::get(record_key(key));
        if let Some(names) = projection {
            operation = operation.only(names.iter().map(String::as_bytes));
        }
        match self.database.get(target, operation) {
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

    /// Out of scope here, and GolemDB cannot yet run Arkiv's queries, which
    /// always combine predicates.
    fn query(
        &self,
        _at: Option<CommitId>,
        _query: &Query,
        _budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        Err(StoreError::NotImplemented)
    }

    fn count(
        &self,
        _at: Option<CommitId>,
        _filter: &Filter,
        _budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        Err(StoreError::NotImplemented)
    }

    /// A constant. The one production caller discards it, and GolemDB has no
    /// digest of an unsealed branch.
    fn branch_hash(&self, _branch: BranchId) -> Result<[u8; 32], StoreError> {
        Ok([0; 32])
    }
}

#[cfg(test)]
mod tests;
