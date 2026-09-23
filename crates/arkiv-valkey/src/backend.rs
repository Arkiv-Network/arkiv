//! The asynchronous half: every Valkey round-trip lives here.
//!
//! One method per [`Store`](arkiv_interfaces::store::Store) operation, so a
//! trait call is one sequence of commands and the synchronous wrapper in
//! [`crate`] stays a thin translation.
//!
//! **Atomicity.** These sequences are not transactional. A single sequencer
//! executing serially is the only writer, so nothing interleaves in practice —
//! but a crash mid-`commit` can leave the head advanced without its undo log.
//! That is the two-durability-domain problem the architecture review already
//! names, and it is why this crate is an interim store rather than a
//! production one.

use std::collections::{BTreeMap, HashMap};

use arkiv_interfaces::store::{
    BranchId, BranchInfo, BranchVersion, Cell, CellChange, CellName, CommitId, Origin, ReadTarget,
    RecordChange, RecordKey, RecordVersion, StoreError, validate_cell_name,
};
use fred::prelude::*;

use crate::codec::{self, Stored};
use crate::keys::{self, Namespace, VERSION_FIELD};

/// A cloneable handle to one namespaced store on one server.
#[derive(Debug, Clone)]
pub(crate) struct Backend {
    client: Client,
    namespace: Namespace,
}

/// Anything the server or an encoding can do wrong becomes
/// [`StoreError::Internal`]: it is not a caller's mistake, and the trait's
/// error set has no richer home for it.
fn internal<E>(_error: E) -> StoreError {
    StoreError::Internal
}

impl Backend {
    pub(crate) const fn new(client: Client, namespace: Namespace) -> Self {
        Self { client, namespace }
    }

    pub(crate) fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    pub(crate) const fn client(&self) -> &Client {
        &self.client
    }

    /// Drop every branch in this namespace.
    ///
    /// Branches are volatile by definition, so ones surviving a restart are
    /// stale by construction. Called once at connect.
    pub(crate) async fn clear_branches(&self) -> Result<(), StoreError> {
        let live: Vec<String> = self
            .client
            .smembers(self.namespace.branches())
            .await
            .map_err(internal)?;
        for text in live {
            if let Ok(id) = text.parse::<u64>() {
                self.drop_branch(BranchId(id)).await?;
            }
        }
        Ok(())
    }

    // -- primitives --------------------------------------------------------

    /// The canonical head. Absent means genesis: the counter is only written
    /// once something commits.
    pub(crate) async fn head(&self) -> Result<u64, StoreError> {
        let head: Option<u64> = self
            .client
            .get(self.namespace.head())
            .await
            .map_err(internal)?;
        Ok(head.unwrap_or(0))
    }

    /// The committed record at `key` as of head.
    async fn committed_record(&self, key: RecordKey) -> Result<Option<Stored>, StoreError> {
        let fields: HashMap<String, Vec<u8>> = self
            .client
            .hgetall(self.namespace.record(key))
            .await
            .map_err(internal)?;
        Ok(decode_record_hash(fields))
    }

    /// Every committed record as of head.
    async fn committed_state(&self) -> Result<BTreeMap<RecordKey, Stored>, StoreError> {
        let hexes: Vec<String> = self
            .client
            .smembers(self.namespace.records())
            .await
            .map_err(internal)?;

        let mut state = BTreeMap::new();
        for text in hexes {
            let Some(key) = keys::record_key_from_hex(&text) else {
                return Err(StoreError::Internal);
            };
            if let Some(record) = self.committed_record(key).await? {
                state.insert(key, record);
            }
        }
        Ok(state)
    }

    /// The state as of `commit`, replaying undo logs backwards when it is not
    /// head.
    ///
    /// Linear in the number of changes between `commit` and head. Acceptable
    /// for an interim store with a short retention window; a real one answers
    /// this from its own history rather than by replay.
    pub(crate) async fn state_at(
        &self,
        commit: CommitId,
    ) -> Result<BTreeMap<RecordKey, Stored>, StoreError> {
        let head = self.head().await?;
        if commit.0 > head {
            return Err(StoreError::NotFound);
        }

        let mut state = self.committed_state().await?;
        // Walk back from head, undoing each commit in turn.
        for id in ((commit.0 + 1)..=head).rev() {
            for change in self.changes(CommitId(id)).await? {
                match change.before {
                    Some(record) => state.insert(change.key, stored_from_record(&record)),
                    None => state.remove(&change.key),
                };
            }
        }
        Ok(state)
    }

    // -- branches ----------------------------------------------------------

    async fn branch_exists(&self, branch: BranchId) -> Result<bool, StoreError> {
        let live: bool = self
            .client
            .sismember(self.namespace.branches(), branch.0.to_string())
            .await
            .map_err(internal)?;
        Ok(live)
    }

    pub(crate) async fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        let meta: HashMap<String, String> = self
            .client
            .hgetall(self.namespace.branch_meta(branch.0))
            .await
            .map_err(internal)?;
        if meta.is_empty() {
            return Err(StoreError::HandleInvalid);
        }

        let field = |name: &str| -> Result<u64, StoreError> {
            meta.get(name)
                .ok_or(StoreError::Internal)?
                .parse()
                .map_err(internal)
        };
        let origin = match meta.get("origin").map(String::as_str) {
            Some("commit") => Origin::Commit(CommitId(field("origin_id")?)),
            Some("branch") => Origin::Branch(
                BranchId(field("origin_id")?),
                BranchVersion(field("origin_version")?),
            ),
            _ => return Err(StoreError::Internal),
        };
        Ok(BranchInfo {
            origin,
            version: BranchVersion(field("version")?),
            depth: field("depth")? as u32,
        })
    }

    /// The origin commit a branch ultimately reads through, following the
    /// parent chain up to its root.
    async fn grounding_commit(&self, branch: BranchId) -> Result<CommitId, StoreError> {
        let mut current = branch;
        loop {
            match self.branch_info(current).await?.origin {
                Origin::Commit(commit) => return Ok(commit),
                Origin::Branch(parent, _) => current = parent,
            }
        }
    }

    /// A branch's staged writes: record key to record, or `None` for a
    /// tombstone.
    async fn branch_diff(
        &self,
        branch: BranchId,
    ) -> Result<BTreeMap<RecordKey, Option<Stored>>, StoreError> {
        let raw: HashMap<String, Vec<u8>> = self
            .client
            .hgetall(self.namespace.branch_diff(branch.0))
            .await
            .map_err(internal)?;

        let mut diff = BTreeMap::new();
        for (text, encoded) in raw {
            let key = keys::record_key_from_hex(&text).ok_or(StoreError::Internal)?;
            diff.insert(key, codec::decode_record(&encoded).map_err(internal)?);
        }
        Ok(diff)
    }

    /// Stage one record — or its deletion — into a branch's diff.
    async fn stage(
        &self,
        branch: BranchId,
        key: RecordKey,
        record: Option<&Stored>,
    ) -> Result<(), StoreError> {
        let _: u64 = self
            .client
            .hset(
                self.namespace.branch_diff(branch.0),
                (keys::hex(&key.0), codec::encode_record(record)),
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// Count one mutation batch against a branch, returning its new version.
    async fn bump_version(&self, branch: BranchId) -> Result<BranchVersion, StoreError> {
        let version: u64 = self
            .client
            .hincrby(self.namespace.branch_meta(branch.0), "version", 1)
            .await
            .map_err(internal)?;
        Ok(BranchVersion(version))
    }

    /// Read one record as a branch sees it: its own staged write if it has
    /// one, else the state it is grounded on.
    pub(crate) async fn read(
        &self,
        target: ReadTarget,
        key: RecordKey,
    ) -> Result<Option<Stored>, StoreError> {
        match target {
            ReadTarget::Branch(branch) => {
                if !self.branch_exists(branch).await? {
                    return Err(StoreError::HandleInvalid);
                }
                let staged: Option<Vec<u8>> = self
                    .client
                    .hget(self.namespace.branch_diff(branch.0), keys::hex(&key.0))
                    .await
                    .map_err(internal)?;
                if let Some(encoded) = staged {
                    return codec::decode_record(&encoded).map_err(internal);
                }
                let origin = self.grounding_commit(branch).await?;
                // Boxed: this is the branch -> origin-commit fall-through, so
                // `read` is recursive and the future must have a known size.
                Box::pin(self.read(ReadTarget::Commit(origin), key)).await
            }
            ReadTarget::Commit(commit) => {
                if commit.0 == self.head().await? {
                    return self.committed_record(key).await;
                }
                Ok(self.state_at(commit).await?.get(&key).cloned())
            }
        }
    }

    /// Open a root branch over `at`, defaulting to head.
    pub(crate) async fn begin(&self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        let head = self.head().await?;
        let origin = at.unwrap_or(CommitId(head));
        if origin.0 > head {
            return Err(StoreError::NotFound);
        }
        self.open_branch(Origin::Commit(origin), 0).await
    }

    /// Open a child branch over `parent`, copying its staged writes.
    ///
    /// The spec makes `fork` O(1) and the child's view immune to later parent
    /// writes. Copying gets the second property — which is the observable one
    /// — at the cost of the first. A block's diff is bounded by that block's
    /// writes, so the copy is small; a real store layers instead.
    pub(crate) async fn fork(&self, parent: BranchId) -> Result<BranchId, StoreError> {
        let info = self.branch_info(parent).await?;
        let child = self
            .open_branch(Origin::Branch(parent, info.version), info.depth + 1)
            .await?;

        let diff = self.branch_diff(parent).await?;
        for (key, record) in diff {
            self.stage(child, key, record.as_ref()).await?;
        }
        Ok(child)
    }

    async fn open_branch(&self, origin: Origin, depth: u32) -> Result<BranchId, StoreError> {
        let id: u64 = self
            .client
            .incr(self.namespace.next_branch())
            .await
            .map_err(internal)?;
        // INCR returns 1 first, and branch ids start at 0.
        let branch = BranchId(id - 1);

        let (kind, origin_id, origin_version) = match origin {
            Origin::Commit(commit) => ("commit", commit.0, 0),
            Origin::Branch(parent, version) => ("branch", parent.0, version.0),
        };
        let _: u64 = self
            .client
            .hset(
                self.namespace.branch_meta(branch.0),
                vec![
                    ("origin", kind.to_owned()),
                    ("origin_id", origin_id.to_string()),
                    ("origin_version", origin_version.to_string()),
                    ("version", "0".to_owned()),
                    ("depth", depth.to_string()),
                ],
            )
            .await
            .map_err(internal)?;
        let _: u64 = self
            .client
            .sadd(self.namespace.branches(), branch.0.to_string())
            .await
            .map_err(internal)?;
        Ok(branch)
    }

    /// Fold a child's staged writes into its parent and consume the handle.
    pub(crate) async fn merge(&self, child: BranchId) -> Result<BranchVersion, StoreError> {
        let info = self.branch_info(child).await?;
        let Origin::Branch(parent, forked_at) = info.origin else {
            // A root branch commits; it does not merge.
            return Err(StoreError::HandleInvalid);
        };
        if self.branch_info(parent).await?.version != forked_at {
            return Err(StoreError::Conflict);
        }

        // The child began as a copy of the parent and diverged only by its own
        // writes, so adopting its diff wholesale is the fold.
        let diff = self.branch_diff(child).await?;
        let _: u64 = self
            .client
            .del(self.namespace.branch_diff(parent.0))
            .await
            .map_err(internal)?;
        for (key, record) in diff {
            self.stage(parent, key, record.as_ref()).await?;
        }

        self.drop_branch(child).await?;
        self.bump_version(parent).await
    }

    /// Drop a branch and every open descendant.
    pub(crate) async fn discard(&self, branch: BranchId) -> Result<(), StoreError> {
        if !self.branch_exists(branch).await? {
            return Err(StoreError::HandleInvalid);
        }

        let live: Vec<String> = self
            .client
            .smembers(self.namespace.branches())
            .await
            .map_err(internal)?;
        self.drop_branch(branch).await?;

        for text in live {
            let Ok(id) = text.parse::<u64>() else {
                continue;
            };
            let candidate = BranchId(id);
            if candidate == branch {
                continue;
            }
            // Descendants are grounded on a handle that no longer exists.
            if let Ok(info) = self.branch_info(candidate).await
                && matches!(info.origin, Origin::Branch(parent, _) if parent == branch)
            {
                Box::pin(self.discard(candidate)).await?;
            }
        }
        Ok(())
    }

    async fn drop_branch(&self, branch: BranchId) -> Result<(), StoreError> {
        let _: u64 = self
            .client
            .del(vec![
                self.namespace.branch_diff(branch.0),
                self.namespace.branch_meta(branch.0),
            ])
            .await
            .map_err(internal)?;
        let _: u64 = self
            .client
            .srem(self.namespace.branches(), branch.0.to_string())
            .await
            .map_err(internal)?;
        Ok(())
    }

    // -- writes ------------------------------------------------------------

    pub(crate) async fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
    ) -> Result<RecordKey, StoreError> {
        if cells.is_empty() {
            return Err(StoreError::InvalidArgument);
        }
        for (name, cell) in &cells {
            validate_cell_name(name)?;
            cell.validate()?;
        }
        if !self.branch_exists(branch).await? {
            return Err(StoreError::HandleInvalid);
        }

        self.bump_version(branch).await?;
        if self.read(ReadTarget::Branch(branch), key).await?.is_some() {
            return Err(StoreError::AlreadyExists);
        }
        self.stage(
            branch,
            key,
            Some(&Stored {
                version: RecordVersion(1),
                cells: cells.into_iter().collect(),
            }),
        )
        .await?;
        Ok(key)
    }

    pub(crate) async fn patch(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
    ) -> Result<RecordVersion, StoreError> {
        for (name, change) in &changes {
            validate_cell_name(name)?;
            if let CellChange::Set(cell) = change {
                cell.validate()?;
            }
        }
        if !self.branch_exists(branch).await? {
            return Err(StoreError::HandleInvalid);
        }

        self.bump_version(branch).await?;
        let mut record = self
            .read(ReadTarget::Branch(branch), key)
            .await?
            .ok_or(StoreError::NotFound)?;
        if expected_version.is_some_and(|expected| expected != record.version) {
            return Err(StoreError::Conflict);
        }

        // Staged against a copy: "may not remove the last cell" must leave the
        // record untouched when it trips, not half-patched.
        let mut staged = record.cells.clone();
        for (name, change) in changes {
            match change {
                CellChange::Set(cell) => staged.insert(name, cell),
                CellChange::Remove => staged.remove(&name),
            };
        }
        if staged.is_empty() {
            return Err(StoreError::InvalidArgument);
        }

        record.cells = staged;
        record.version = RecordVersion(record.version.0 + 1);
        self.stage(branch, key, Some(&record)).await?;
        Ok(record.version)
    }

    pub(crate) async fn delete(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
    ) -> Result<(), StoreError> {
        if !self.branch_exists(branch).await? {
            return Err(StoreError::HandleInvalid);
        }
        self.bump_version(branch).await?;

        let record = self
            .read(ReadTarget::Branch(branch), key)
            .await?
            .ok_or(StoreError::NotFound)?;
        if expected_version.is_some_and(|expected| expected != record.version) {
            return Err(StoreError::Conflict);
        }
        self.stage(branch, key, None).await
    }

    /// Replace a branch's staged writes wholesale — the rollback half of an
    /// all-or-nothing batch.
    pub(crate) async fn restore_diff(
        &self,
        branch: BranchId,
        diff: BTreeMap<RecordKey, Option<Stored>>,
        version: BranchVersion,
    ) -> Result<(), StoreError> {
        let _: u64 = self
            .client
            .del(self.namespace.branch_diff(branch.0))
            .await
            .map_err(internal)?;
        for (key, record) in diff {
            self.stage(branch, key, record.as_ref()).await?;
        }
        let _: u64 = self
            .client
            .hset(
                self.namespace.branch_meta(branch.0),
                ("version", version.0.to_string()),
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// A branch's staged writes and current version, for batch rollback.
    pub(crate) async fn snapshot_diff(
        &self,
        branch: BranchId,
    ) -> Result<(BTreeMap<RecordKey, Option<Stored>>, BranchVersion), StoreError> {
        let version = self.branch_info(branch).await?.version;
        Ok((self.branch_diff(branch).await?, version))
    }

    pub(crate) async fn set_version(
        &self,
        branch: BranchId,
        version: BranchVersion,
    ) -> Result<(), StoreError> {
        let _: u64 = self
            .client
            .hset(
                self.namespace.branch_meta(branch.0),
                ("version", version.0.to_string()),
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    // -- commit ------------------------------------------------------------

    /// Seal a root branch. `tag` attaches an opaque host identifier.
    pub(crate) async fn commit(
        &self,
        root: BranchId,
        tag: Option<[u8; 32]>,
    ) -> Result<CommitId, StoreError> {
        let info = self.branch_info(root).await?;
        let Origin::Commit(origin) = info.origin else {
            // Child branches may only merge or discard.
            return Err(StoreError::HandleInvalid);
        };
        let head = self.head().await?;
        // The no-fork guarantee: a branch whose origin has been overtaken
        // cannot seal.
        if origin.0 != head {
            return Err(StoreError::Conflict);
        }

        let diff = self.branch_diff(root).await?;
        let mut digest = self.digest().await?;
        let mut changes = Vec::new();

        for (key, after) in diff {
            let before = self.committed_record(key).await?;
            if before.as_ref().map(|r| &r.cells) == after.as_ref().map(|r| &r.cells) {
                // Content-equal: not a change, and it must not move the digest.
                continue;
            }
            if let Some(record) = &before {
                codec::fold_digest(&mut digest, codec::record_digest(key, record));
            }
            if let Some(record) = &after {
                codec::fold_digest(&mut digest, codec::record_digest(key, record));
            }
            changes.push(codec::encode_change(key, before.as_ref(), after.as_ref()));
            self.write_committed(key, after.as_ref()).await?;
        }

        let commit = CommitId(head + 1);
        if !changes.is_empty() {
            let _: u64 = self
                .client
                .rpush(self.namespace.commit_undo(commit.0), changes)
                .await
                .map_err(internal)?;
        }
        let _: Option<()> = self
            .client
            .set(
                self.namespace.commit_hash(commit.0),
                digest.to_vec(),
                None,
                None,
                false,
            )
            .await
            .map_err(internal)?;
        let _: Option<()> = self
            .client
            .set(self.namespace.digest(), digest.to_vec(), None, None, false)
            .await
            .map_err(internal)?;
        if let Some(tag) = tag {
            let _: Option<()> = self
                .client
                .set(
                    self.namespace.commit_tag(commit.0),
                    tag.to_vec(),
                    None,
                    None,
                    false,
                )
                .await
                .map_err(internal)?;
            let _: Option<()> = self
                .client
                .set(
                    self.namespace.tag_index(tag),
                    commit.0.to_string(),
                    None,
                    None,
                    false,
                )
                .await
                .map_err(internal)?;
        }
        let _: Option<()> = self
            .client
            .set(
                self.namespace.head(),
                commit.0.to_string(),
                None,
                None,
                false,
            )
            .await
            .map_err(internal)?;

        self.drop_branch(root).await?;
        Ok(commit)
    }

    /// Write one record into committed state, keeping the record index in step.
    async fn write_committed(
        &self,
        key: RecordKey,
        record: Option<&Stored>,
    ) -> Result<(), StoreError> {
        match record {
            Some(record) => {
                let _: u64 = self
                    .client
                    .del(self.namespace.record(key))
                    .await
                    .map_err(internal)?;
                let mut fields: Vec<(String, Vec<u8>)> = record
                    .cells
                    .iter()
                    .map(|(name, cell)| {
                        let mut value = vec![cell.tag()];
                        value.extend_from_slice(&cell.value);
                        (name.clone(), value)
                    })
                    .collect();
                fields.push((
                    VERSION_FIELD.to_owned(),
                    record.version.0.to_string().into_bytes(),
                ));
                let _: u64 = self
                    .client
                    .hset(self.namespace.record(key), fields)
                    .await
                    .map_err(internal)?;
                let _: u64 = self
                    .client
                    .sadd(self.namespace.records(), keys::hex(&key.0))
                    .await
                    .map_err(internal)?;
            }
            None => {
                let _: u64 = self
                    .client
                    .del(self.namespace.record(key))
                    .await
                    .map_err(internal)?;
                let _: u64 = self
                    .client
                    .srem(self.namespace.records(), keys::hex(&key.0))
                    .await
                    .map_err(internal)?;
            }
        }
        Ok(())
    }

    /// The committed state digest. Absent means the empty state, which is all
    /// zeros because the accumulator is an XOR fold.
    async fn digest(&self) -> Result<[u8; 32], StoreError> {
        let raw: Option<Vec<u8>> = self
            .client
            .get(self.namespace.digest())
            .await
            .map_err(internal)?;
        fixed_32(raw)
    }

    /// A branch's digest: the state it is grounded on, with its staged writes
    /// folded in. Linear in the size of the diff, not of the state.
    pub(crate) async fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError> {
        let origin = self.grounding_commit(branch).await?;
        let mut digest = if origin.0 == self.head().await? {
            self.digest().await?
        } else {
            self.commit_hash(origin).await?
        };

        for (key, after) in self.branch_diff(branch).await? {
            let before = self.read(ReadTarget::Commit(origin), key).await?;
            if before.as_ref().map(|r| &r.cells) == after.as_ref().map(|r| &r.cells) {
                continue;
            }
            if let Some(record) = &before {
                codec::fold_digest(&mut digest, codec::record_digest(key, record));
            }
            if let Some(record) = &after {
                codec::fold_digest(&mut digest, codec::record_digest(key, record));
            }
        }
        Ok(digest)
    }

    pub(crate) async fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        if commit == CommitId::GENESIS {
            return Ok([0u8; 32]);
        }
        let raw: Option<Vec<u8>> = self
            .client
            .get(self.namespace.commit_hash(commit.0))
            .await
            .map_err(internal)?;
        if raw.is_none() {
            return Err(StoreError::NotFound);
        }
        fixed_32(raw)
    }

    pub(crate) async fn commit_by_tag(
        &self,
        tag: [u8; 32],
    ) -> Result<Option<CommitId>, StoreError> {
        let found: Option<u64> = self
            .client
            .get(self.namespace.tag_index(tag))
            .await
            .map_err(internal)?;
        Ok(found.map(CommitId))
    }

    pub(crate) async fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        if commit.0 > self.head().await? {
            return Err(StoreError::NotFound);
        }
        let raw: Vec<Vec<u8>> = self
            .client
            .lrange(self.namespace.commit_undo(commit.0), 0, -1)
            .await
            .map_err(internal)?;

        let mut changes = Vec::with_capacity(raw.len());
        for encoded in raw {
            let (key, before, after) = codec::decode_change(&encoded).map_err(internal)?;
            changes.push(RecordChange {
                key,
                before: before.map(|record| record.to_record(key, None)),
                after: after.map(|record| record.to_record(key, None)),
            });
        }
        changes.sort_by_key(|change| change.key);
        Ok(changes)
    }
}

/// Rebuild a record from its committed hash, or `None` if the hash is absent.
fn decode_record_hash(mut fields: HashMap<String, Vec<u8>>) -> Option<Stored> {
    let version = fields.remove(VERSION_FIELD)?;
    let version = RecordVersion(
        core::str::from_utf8(&version)
            .ok()
            .and_then(|text| text.parse().ok())?,
    );

    let mut cells = BTreeMap::new();
    for (name, raw) in fields {
        let (tag, value) = raw.split_first()?;
        let (kind, type_id) = Cell::split_tag(*tag)?;
        cells.insert(
            name,
            Cell {
                kind,
                type_id,
                value: value.to_vec(),
            },
        );
    }
    Some(Stored { version, cells })
}

/// Turn a caller-facing [`Record`](arkiv_interfaces::store::Record) back into
/// storage form — the inverse of [`Stored::to_record`], used when replaying
/// changesets.
fn stored_from_record(record: &arkiv_interfaces::store::Record) -> Stored {
    Stored {
        version: record.version,
        cells: record.cells.iter().cloned().collect(),
    }
}

/// A stored digest must be exactly 32 bytes; absent means the empty state.
fn fixed_32(raw: Option<Vec<u8>>) -> Result<[u8; 32], StoreError> {
    match raw {
        None => Ok([0u8; 32]),
        Some(bytes) => bytes.try_into().map_err(|_| StoreError::Internal),
    }
}
