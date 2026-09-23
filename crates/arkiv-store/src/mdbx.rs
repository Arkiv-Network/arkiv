//! [`ArkivDb`]: the durable node store, one MDBX table `hash -> rlp(node)`
//! under the datadir, with a cache of raw nodes in front. Nodes are
//! immutable, so the cache never invalidates.
//!
//! One MDBX environment may be opened once per process, so [`ArkivDb::shared`]
//! hands out one handle per path.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use alloy_primitives::B256;
use arkiv_trie::{MemNodeStore, NodeReader, NodeStore};
use reth_libmdbx::{
    DatabaseFlags, Environment, EnvironmentFlags, Geometry, Mode, SyncMode, WriteFlags,
};

const NODE_TABLE: &str = "arkiv_nodes";
const CACHE_NODES: usize = 200_000;

/// The durable node store.
pub struct ArkivDb {
    env: Environment,
    dbi: u32,
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
    Mdbx(reth_libmdbx::Error),
}

impl core::fmt::Display for DbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mdbx(e) => write!(f, "mdbx: {e}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<reth_libmdbx::Error> for DbError {
    fn from(e: reth_libmdbx::Error) -> Self {
        Self::Mdbx(e)
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<ArkivDb>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<ArkivDb>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

impl ArkivDb {
    /// Open (or create) the store at `path`, a directory.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        std::fs::create_dir_all(path).map_err(|_| reth_libmdbx::Error::Other(-1))?;
        let mut builder = Environment::builder();
        builder.set_max_dbs(4);
        builder.set_geometry(Geometry {
            size: Some(0..(1 << 40)),
            growth_step: Some(1 << 30),
            shrink_threshold: None,
            page_size: None,
        });
        builder.set_flags(EnvironmentFlags {
            mode: Mode::ReadWrite {
                sync_mode: SyncMode::Durable,
            },
            no_rdahead: true,
            coalesce: true,
            ..Default::default()
        });
        builder.set_max_readers(1024);
        let env = builder.open(path)?;
        let dbi = {
            let txn = env.begin_rw_txn()?;
            let db = txn.create_db(Some(NODE_TABLE), DatabaseFlags::empty())?;
            let dbi = db.dbi();
            txn.commit()?;
            dbi
        };
        Ok(Self {
            env,
            dbi,
            cache: parking_lot::Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(CACHE_NODES).expect("non-zero"),
            )),
            path: path.to_path_buf(),
        })
    }

    /// The process-wide handle for `path`: opened on first use, shared after.
    pub fn shared(path: &Path) -> Result<Arc<Self>, DbError> {
        let mut reg = registry().lock().expect("registry poisoned");
        if let Some(db) = reg.get(path).and_then(Weak::upgrade) {
            return Ok(db);
        }
        let db = Arc::new(Self::open(path)?);
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
        let txn = self.env.begin_rw_txn()?;
        for (hash, rlp) in staged.iter() {
            match txn.put(self.dbi, hash.as_slice(), rlp, WriteFlags::NO_OVERWRITE) {
                Ok(()) | Err(reth_libmdbx::Error::KeyExist) => {}
                Err(e) => return Err(e.into()),
            }
        }
        txn.commit()?;
        let mut cache = self.cache.lock();
        for (hash, rlp) in staged.iter() {
            cache.put(*hash, rlp.clone());
        }
        Ok(())
    }

    /// How many nodes the table holds.
    pub fn node_count(&self) -> Result<usize, DbError> {
        let txn = self.env.begin_ro_txn()?;
        Ok(txn.db_stat_with_dbi(self.dbi)?.entries())
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
        let txn = self.env.begin_ro_txn()?;
        let found: Option<Vec<u8>> = txn.get(self.dbi, hash.as_slice())?;
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

    #[test]
    fn nodes_survive_a_flush_and_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arkiv-db");
        let root = {
            let db = ArkivDb::shared(&path).unwrap();
            let again = ArkivDb::shared(&path).unwrap();
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
        let db = ArkivDb::shared(&path).unwrap();
        let view = DbView::open(&db, root).unwrap();
        assert_eq!(view.entity(&[1; 32]).unwrap().unwrap().payload, b"hello");
    }
}
