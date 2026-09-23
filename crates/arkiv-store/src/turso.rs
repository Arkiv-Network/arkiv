//! [`ArkivDb`]: the durable node store, one turso (SQLite-style) table
//! `hash -> rlp(node)` under the datadir, with a cache of raw nodes in
//! front. Nodes are immutable, so the cache never invalidates.
//!
//! The trie never sees the engine: it reads through `NodeReader` and flushes
//! through `NodeStore`. This file is the whole engine binding, and swapping it
//! for another key-value store touches nothing else.
//!
//! turso is async and the trie's callers are not, so every call runs on the
//! runtime handle the store was opened with, the way the pruning map does.
//! [`ArkivDb::shared`] hands out one handle per path.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use alloy_primitives::B256;
use arkiv_trie::{MemNodeStore, NodeReader, NodeStore};
use tokio::runtime::Handle;

const CACHE_NODES: usize = 200_000;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS nodes (hash BLOB PRIMARY KEY, rlp BLOB NOT NULL)";

/// The durable node store.
pub struct ArkivDb {
    database: turso::Database,
    runtime: Handle,
    /// One writer at a time: a flush is one transaction, and the engine
    /// serializes writers anyway.
    write_lock: Mutex<()>,
    cache: parking_lot::Mutex<lru::LruCache<B256, Vec<u8>>>,
    path: PathBuf,
}

impl core::fmt::Debug for ArkivDb {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ArkivDb").field("path", &self.path).finish()
    }
}

#[derive(Debug)]
pub enum DbError {
    Turso(turso::Error),
    Io(std::io::Error),
    Path(PathBuf),
}

impl core::fmt::Display for DbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Turso(e) => write!(f, "turso: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Path(p) => write!(f, "node store path is not UTF-8: {}", p.display()),
        }
    }
}

impl std::error::Error for DbError {}

impl From<turso::Error> for DbError {
    fn from(e: turso::Error) -> Self {
        Self::Turso(e)
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<ArkivDb>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<ArkivDb>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Run `fut` to completion from synchronous code. Inside a multi-thread
/// runtime the worker is released first; on any other thread the handle
/// drives the future directly.
fn run<F: Future>(runtime: &Handle, fut: F) -> F::Output {
    if Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| runtime.block_on(fut))
    } else {
        runtime.block_on(fut)
    }
}

impl ArkivDb {
    /// Open (or create) the store in the directory `path`.
    pub fn open(path: &Path, runtime: Handle) -> Result<Self, DbError> {
        std::fs::create_dir_all(path).map_err(DbError::Io)?;
        let file = path.join("nodes.db");
        let file = file
            .to_str()
            .ok_or_else(|| DbError::Path(file.clone()))?
            .to_owned();
        let database = run(&runtime, async {
            let database = turso::Builder::new_local(&file).build().await?;
            database.connect()?.execute_batch(SCHEMA).await?;
            Ok::<_, turso::Error>(database)
        })?;
        Ok(Self {
            database,
            runtime,
            write_lock: Mutex::new(()),
            cache: parking_lot::Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(CACHE_NODES).expect("non-zero"),
            )),
            path: path.to_path_buf(),
        })
    }

    /// The process-wide handle for `path`: opened on first use, shared after.
    pub fn shared(path: &Path, runtime: Handle) -> Result<Arc<Self>, DbError> {
        let mut reg = registry().lock().expect("registry poisoned");
        if let Some(db) = reg.get(path).and_then(Weak::upgrade) {
            return Ok(db);
        }
        let db = Arc::new(Self::open(path, runtime)?);
        reg.insert(path.to_path_buf(), Arc::downgrade(&db));
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write every staged node in one transaction. A node already present is
    /// left alone: same hash, same bytes.
    pub fn flush(&self, staged: MemNodeStore) -> Result<(), DbError> {
        if staged.is_empty() {
            return Ok(());
        }
        let _writer = self.write_lock.lock().expect("write lock poisoned");
        run(&self.runtime, async {
            let connection = self.database.connect()?;
            connection.execute("BEGIN IMMEDIATE", ()).await?;
            for (hash, rlp) in staged.iter() {
                connection
                    .execute(
                        "INSERT OR IGNORE INTO nodes (hash, rlp) VALUES (?1, ?2)",
                        turso::params![hash.as_slice(), rlp.as_slice()],
                    )
                    .await?;
            }
            connection.execute("COMMIT", ()).await?;
            Ok::<_, turso::Error>(())
        })?;
        let mut cache = self.cache.lock();
        for (hash, rlp) in staged.iter() {
            cache.put(*hash, rlp.clone());
        }
        Ok(())
    }

    /// How many nodes the table holds.
    pub fn node_count(&self) -> Result<usize, DbError> {
        let count = run(&self.runtime, async {
            let connection = self.database.connect()?;
            let row = connection
                .query("SELECT COUNT(*) FROM nodes", ())
                .await?
                .next()
                .await?
                .expect("COUNT returns one row");
            row.get::<i64>(0)
        })?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    fn read(&self, hash: &B256) -> Result<Option<Vec<u8>>, DbError> {
        run(&self.runtime, async {
            let connection = self.database.connect()?;
            let mut rows = connection
                .query(
                    "SELECT rlp FROM nodes WHERE hash = ?1",
                    turso::params![hash.as_slice()],
                )
                .await?;
            match rows.next().await? {
                Some(row) => Ok(Some(row.get::<Vec<u8>>(0)?)),
                None => Ok(None),
            }
        })
        .map_err(DbError::Turso)
    }
}

impl NodeStore for ArkivDb {
    fn flush(&self, staged: MemNodeStore) -> Result<(), DbError> {
        Self::flush(self, staged)
    }
}

impl NodeReader for ArkivDb {
    type Error = DbError;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, DbError> {
        if let Some(n) = self.cache.lock().get(hash) {
            return Ok(Some(n.clone()));
        }
        let found = self.read(hash)?;
        if let Some(n) = &found {
            self.cache.lock().put(*hash, n.clone());
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roots::DbRoots;
    use crate::view::{DbChanges, DbView, commit_changes};
    use arkiv_interfaces::entity::Entity;
    use arkiv_trie::Staging;

    #[tokio::test(flavor = "multi_thread")]
    async fn nodes_survive_a_flush_and_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arkiv-db");
        let runtime = Handle::current();
        let root = {
            let db = ArkivDb::shared(&path, runtime.clone()).unwrap();
            let again = ArkivDb::shared(&path, runtime.clone()).unwrap();
            assert!(Arc::ptr_eq(&db, &again));
            let mut staging = Staging::new(&db);
            let mut changes = DbChanges::default();
            changes.entities.insert(
                [1; 32],
                Some(Entity {
                    key: [1; 32],
                    payload: b"hello".to_vec(),
                    ..Entity::default()
                }),
            );
            let roots = commit_changes(&mut staging, DbRoots::EMPTY, &changes).unwrap();
            db.flush(staging.into_staged()).unwrap();
            // Flushing the same nodes again is a no-op.
            let mut staging = Staging::new(&db);
            commit_changes(&mut staging, DbRoots::EMPTY, &changes).unwrap();
            db.flush(staging.into_staged()).unwrap();
            assert!(db.node_count().unwrap() > 0);
            roots.root()
        };
        // The registry dropped its weak reference: this is a real reopen,
        // and the cache starts empty.
        let db = ArkivDb::shared(&path, runtime).unwrap();
        let view = DbView::open(&db, root).unwrap();
        assert_eq!(view.entity(&[1; 32]).unwrap().unwrap().payload, b"hello");
    }
}
