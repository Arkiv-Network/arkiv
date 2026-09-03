//! Durable pruning candidates and their applied-chain watermark.

use alloy_primitives::B256;
use arkiv_interfaces::gas::purge_cost;
use eyre::{Context, Result, bail, eyre};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{runtime::Handle, sync::Mutex, time::timeout};
use turso::{
    Connection, Database,
    transaction::{Transaction, TransactionBehavior},
};

const SELECT_TIMEOUT: Duration = Duration::from_millis(100);

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS pruning_entries (
    entity_id       BLOB PRIMARY KEY NOT NULL,
    expires_at      INTEGER NOT NULL,
    attribute_count INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS pruning_entries_by_expiry
    ON pruning_entries (expires_at, entity_id);
CREATE TABLE IF NOT EXISTS pruning_metadata (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    up_to_date_at   INTEGER NOT NULL
);
INSERT OR IGNORE INTO pruning_metadata (singleton, up_to_date_at) VALUES (1, 0);
"#;

const SELECT_EXPIRED: &str = r#"
SELECT entity_id, attribute_count
FROM pruning_entries
WHERE expires_at <= ?1
  AND (SELECT up_to_date_at FROM pruning_metadata WHERE singleton = 1) = ?2
ORDER BY expires_at, entity_id
LIMIT ?3
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PruningEntry {
    pub(crate) key: B256,
    pub(crate) expires_at: u64,
    pub(crate) attribute_count: usize,
}

#[derive(Clone)]
pub(crate) struct ChainPruningMap {
    database: Database,
    runtime: Handle,
    update_lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for ChainPruningMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainPruningMap").finish_non_exhaustive()
    }
}

impl ChainPruningMap {
    pub(crate) async fn open(path: &Path, runtime: Handle) -> Result<Self> {
        let path = path
            .to_str()
            .ok_or_else(|| eyre!("pruning map path is not UTF-8: {}", path.display()))?;
        let database = turso::Builder::new_local(path)
            .build()
            .await
            .wrap_err("open pruning map")?;
        database
            .connect()
            .wrap_err("connect to pruning map")?
            .execute_batch(SCHEMA)
            .await
            .wrap_err("initialize pruning map schema")?;
        Ok(Self {
            database,
            runtime,
            update_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Select candidates only when the map includes the requested parent.
    ///
    /// Reth's `PayloadBuilder::try_build` method is synchronous and cannot
    /// await Turso's asynchronous query API. This adapter runs the query on
    /// the node runtime and limits the payload builder delay to
    /// [`SELECT_TIMEOUT`].
    pub(crate) fn select_expired(
        &self,
        parent: u64,
        block: u64,
        limit: usize,
        gas_limit: u64,
    ) -> Result<Vec<B256>> {
        self.runtime.block_on(async {
            timeout(
                SELECT_TIMEOUT,
                self.select_expired_async(parent, block, limit, gas_limit),
            )
            .await
            .map_err(|_| eyre!("pruning map selection exceeded {SELECT_TIMEOUT:?}"))?
        })
    }

    /// Run the Turso query used by the synchronous payload builder adapter.
    ///
    /// Turso exposes asynchronous query and row APIs. Keep that work in this
    /// method so [`Self::select_expired`] only has to bridge the synchronous
    /// `PayloadBuilder::try_build` interface to the node runtime.
    async fn select_expired_async(
        &self,
        parent: u64,
        block: u64,
        limit: usize,
        gas_limit: u64,
    ) -> Result<Vec<B256>> {
        let block = sql_integer(block, "block height")?;
        let parent = sql_integer(parent, "parent height")?;
        let limit = i64::try_from(limit).wrap_err("candidate limit exceeds SQLite INTEGER")?;
        let connection = self.database.connect().wrap_err("connect for selection")?;
        let mut rows = connection
            .query(SELECT_EXPIRED, turso::params![block, parent, limit])
            .await
            .wrap_err("select pruning candidates")?;
        let mut selected = Vec::with_capacity(limit as usize);
        let mut gas = 0u64;
        while let Some(row) = rows.next().await.wrap_err("read pruning candidate")? {
            let key = key_from_blob(row.get::<Vec<u8>>(0).wrap_err("read candidate key")?)?;
            let attribute_count = row
                .get::<i64>(1)
                .wrap_err("read candidate attribute count")?;
            let attribute_count = usize::try_from(attribute_count)
                .wrap_err("candidate attribute count is negative or too large")?;
            let next_gas = gas.saturating_add(purge_cost(attribute_count));
            if next_gas > gas_limit {
                break;
            }
            gas = next_gas;
            selected.push(key);
        }
        Ok(selected)
    }

    pub(crate) async fn watermark(&self) -> Result<u64> {
        watermark(&self.database.connect().wrap_err("connect for watermark")?).await
    }

    /// Apply one block and its watermark as one database transaction.
    pub(crate) async fn apply_next(
        &self,
        height: u64,
        entries: &[PruningEntry],
        removed: &[B256],
    ) -> Result<()> {
        let mut connection = self
            .database
            .connect()
            .wrap_err("connect for block apply")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .wrap_err("begin pruning map transaction")?;
        let current = watermark(&transaction).await?;
        if height != current.saturating_add(1) {
            bail!("cannot apply block {height} after pruning watermark {current}");
        }
        for entry in entries {
            upsert(&transaction, *entry).await?;
        }
        for key in removed {
            transaction
                .execute(
                    "DELETE FROM pruning_entries WHERE entity_id = ?1",
                    turso::params![key.as_slice()],
                )
                .await
                .wrap_err_with(|| format!("remove pruning entry {key}"))?;
        }
        let updated = transaction
            .execute(
                "UPDATE pruning_metadata SET up_to_date_at = ?1 \
                 WHERE singleton = 1 AND up_to_date_at = ?2",
                turso::params![
                    sql_integer(height, "block height")?,
                    sql_integer(current, "watermark")?
                ],
            )
            .await
            .wrap_err("advance pruning watermark")?;
        if updated != 1 {
            bail!("pruning watermark changed while applying block {height}");
        }
        transaction
            .commit()
            .await
            .wrap_err("commit pruning map block")
    }

    pub(crate) async fn update_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.update_lock.lock().await
    }
}

async fn watermark(connection: &Connection) -> Result<u64> {
    let mut rows = connection
        .query(
            "SELECT up_to_date_at FROM pruning_metadata WHERE singleton = 1",
            (),
        )
        .await
        .wrap_err("query pruning watermark")?;
    let row = rows
        .next()
        .await
        .wrap_err("read pruning watermark")?
        .ok_or_else(|| eyre!("pruning watermark row is missing"))?;
    let value = row.get::<i64>(0).wrap_err("decode pruning watermark")?;
    u64::try_from(value).wrap_err("pruning watermark is negative")
}

async fn upsert(transaction: &Transaction<'_>, entry: PruningEntry) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO pruning_entries (entity_id, expires_at, attribute_count) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(entity_id) DO UPDATE SET \
                 expires_at = excluded.expires_at, \
                 attribute_count = excluded.attribute_count",
            turso::params![
                entry.key.as_slice(),
                sql_integer(entry.expires_at, "expiry")?,
                i64::try_from(entry.attribute_count)
                    .wrap_err("attribute count exceeds SQLite INTEGER")?
            ],
        )
        .await
        .wrap_err_with(|| format!("upsert pruning entry {}", entry.key))?;
    Ok(())
}

fn sql_integer(value: u64, name: &str) -> Result<i64> {
    i64::try_from(value).wrap_err_with(|| format!("{name} exceeds SQLite INTEGER"))
}

fn key_from_blob(bytes: Vec<u8>) -> Result<B256> {
    if bytes.len() != B256::len_bytes() {
        bail!("pruning entry key has {} bytes, expected 32", bytes.len());
    }
    Ok(B256::from_slice(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(byte: u8, expires_at: u64, attribute_count: usize) -> PruningEntry {
        PruningEntry {
            key: B256::repeat_byte(byte),
            expires_at,
            attribute_count,
        }
    }

    async fn select(
        map: ChainPruningMap,
        parent: u64,
        block: u64,
        limit: usize,
        gas_limit: u64,
    ) -> Vec<B256> {
        tokio::task::spawn_blocking(move || {
            map.select_expired(parent, block, limit, gas_limit).unwrap()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn persists_entries_and_gates_selection_on_the_watermark() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pruning.db");
        let runtime = Handle::current();
        let map = ChainPruningMap::open(&path, runtime.clone()).await.unwrap();
        map.apply_next(1, &[entry(3, 6, 0), entry(1, 7, 0)], &[])
            .await
            .unwrap();

        assert!(select(map.clone(), 0, 7, 10, u64::MAX).await.is_empty());
        assert_eq!(
            select(map.clone(), 1, 7, 10, u64::MAX).await,
            vec![B256::repeat_byte(3), B256::repeat_byte(1)]
        );

        drop(map);
        let reopened = ChainPruningMap::open(&path, runtime).await.unwrap();
        assert_eq!(reopened.watermark().await.unwrap(), 1);
        assert_eq!(
            select(reopened, 1, 7, 10, u64::MAX).await,
            vec![B256::repeat_byte(3), B256::repeat_byte(1)]
        );
    }

    #[tokio::test]
    async fn reschedules_removes_and_enforces_the_gas_limit() {
        let directory = tempfile::tempdir().unwrap();
        let map = ChainPruningMap::open(&directory.path().join("pruning.db"), Handle::current())
            .await
            .unwrap();
        map.apply_next(1, &[entry(1, 2, 32), entry(2, 2, 32)], &[])
            .await
            .unwrap();
        map.apply_next(2, &[entry(2, 5, 32)], &[B256::repeat_byte(1)])
            .await
            .unwrap();

        assert!(select(map.clone(), 2, 2, 10, 1_000_000).await.is_empty());
        assert_eq!(
            select(map, 2, 5, 10, 170_000).await,
            vec![B256::repeat_byte(2)]
        );
    }
}
