//! Immutable content-addressed records. No mutable "latest root": the host's
//! Ethereum state selects the root, so forks cannot overwrite one another.

use alloy_primitives::B256;
use eyre::{Result, ensure};
use reth_db::mdbx::{DatabaseArguments, DatabaseEnv, create_db};
use reth_db_api::{
    database::Database,
    table::{Table, TableInfo},
    tables::TableSet,
    transaction::{DbTx, DbTxMut},
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, RwLock},
};

pub trait RecordStore: Clone {
    fn get(&self, hash: B256) -> Result<Option<Vec<u8>>>;
    /// All records must be durable on success. A failed write may leave orphan
    /// immutable records, but must never publish a new canonical root.
    fn write(&self, records: &BTreeMap<B256, Vec<u8>>) -> Result<()>;
}

#[derive(Debug, Clone, Default)]
pub struct MemoryStore(Arc<RwLock<BTreeMap<B256, Vec<u8>>>>);

impl RecordStore for MemoryStore {
    fn get(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        Ok(self
            .0
            .read()
            .map_err(|_| eyre::eyre!("poisoned record store"))?
            .get(&hash)
            .cloned())
    }

    fn write(&self, records: &BTreeMap<B256, Vec<u8>>) -> Result<()> {
        self.0
            .write()
            .map_err(|_| eyre::eyre!("poisoned record store"))?
            .extend(records.clone());
        Ok(())
    }
}

#[derive(Debug)]
struct ArkivRecords;

impl Table for ArkivRecords {
    const NAME: &'static str = "ArkivAuthenticatedRecordsV1";
    const DUPSORT: bool = false;
    type Key = B256;
    type Value = Vec<u8>;
}

impl TableInfo for ArkivRecords {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn is_dupsort(&self) -> bool {
        false
    }
}

struct Schema;
impl TableSet for Schema {
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(std::iter::once(Box::new(ArkivRecords) as Box<dyn TableInfo>))
    }
}

/// Node-local immutable records, committed before the native state publishes roots.
#[derive(Debug, Clone)]
pub struct MdbxStore(Arc<DatabaseEnv>);

impl MdbxStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // MDBX requires one environment handle per process/path. Component
        // builders may independently request the same node-local store.
        static OPEN: std::sync::OnceLock<
            std::sync::Mutex<BTreeMap<std::path::PathBuf, std::sync::Weak<DatabaseEnv>>>,
        > = std::sync::OnceLock::new();
        std::fs::create_dir_all(path.as_ref())?;
        let path = std::fs::canonicalize(path)?;
        let mut open = OPEN
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| eyre::eyre!("record-store registry poisoned"))?;
        if let Some(db) = open.get(&path).and_then(std::sync::Weak::upgrade) {
            return Ok(Self(db));
        }
        let mut db = create_db(&path, DatabaseArguments::default())?;
        db.create_and_track_tables_for::<Schema>()?;
        let db = Arc::new(db);
        open.insert(path, Arc::downgrade(&db));
        Ok(Self(db))
    }
}

#[derive(Debug, Clone)]
pub enum Store {
    Memory(MemoryStore),
    Disk(MdbxStore),
}

impl Default for Store {
    fn default() -> Self {
        Self::Memory(MemoryStore::default())
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::Disk(MdbxStore::open(path)?))
    }
}

impl RecordStore for Store {
    fn get(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Memory(s) => s.get(hash),
            Self::Disk(s) => s.get(hash),
        }
    }
    fn write(&self, records: &BTreeMap<B256, Vec<u8>>) -> Result<()> {
        match self {
            Self::Memory(s) => s.write(records),
            Self::Disk(s) => s.write(records),
        }
    }
}

impl RecordStore for MdbxStore {
    fn get(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        Ok(self.0.tx()?.get::<ArkivRecords>(hash)?)
    }

    fn write(&self, records: &BTreeMap<B256, Vec<u8>>) -> Result<()> {
        let tx = self.0.tx_mut()?;
        for (hash, bytes) in records {
            if let Some(existing) = tx.get::<ArkivRecords>(*hash)? {
                ensure!(existing == *bytes, "immutable record conflict at {hash}");
            } else {
                tx.put::<ArkivRecords>(*hash, bytes.clone())?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
