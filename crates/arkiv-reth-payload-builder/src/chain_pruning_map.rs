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
CREATE TABLE IF NOT EXISTS pruning_bootstrap (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    genesis_done    INTEGER NOT NULL
);
INSERT OR IGNORE INTO pruning_bootstrap (singleton, genesis_done) VALUES (1, 0);
CREATE TABLE IF NOT EXISTS pruning_bootstrap_cursor (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    next_id         INTEGER NOT NULL
);
INSERT OR IGNORE INTO pruning_bootstrap_cursor (singleton, next_id) VALUES (1, 0);
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

    /// Whether the entities present at genesis have been folded in. They never
    /// appear in a block, so the per-block replay cannot discover them; a
    /// one-time walk of the genesis state does, and this records that it ran.
    #[allow(dead_code)] // the genesis walk went with seeding
    pub(crate) async fn genesis_bootstrapped(&self) -> Result<bool> {
        let connection = self
            .database
            .connect()
            .wrap_err("connect for bootstrap flag")?;
        let mut rows = connection
            .query(
                "SELECT genesis_done FROM pruning_bootstrap WHERE singleton = 1",
                (),
            )
            .await
            .wrap_err("query pruning bootstrap flag")?;
        let row = rows
            .next()
            .await
            .wrap_err("read pruning bootstrap flag")?
            .ok_or_else(|| eyre!("pruning bootstrap row is missing"))?;
        Ok(row
            .get::<i64>(0)
            .wrap_err("decode pruning bootstrap flag")?
            != 0)
    }

    /// Where an unfinished genesis walk resumes: the first entity id it has
    /// not folded in yet. Zero before the walk starts.
    #[allow(dead_code)] // the genesis walk went with seeding
    pub(crate) fn genesis_cursor_blocking(&self) -> Result<u64> {
        self.runtime.block_on(async {
            let connection = self
                .database
                .connect()
                .wrap_err("connect for bootstrap cursor")?;
            let mut rows = connection
                .query(
                    "SELECT next_id FROM pruning_bootstrap_cursor WHERE singleton = 1",
                    (),
                )
                .await
                .wrap_err("query pruning bootstrap cursor")?;
            let row = rows
                .next()
                .await
                .wrap_err("read pruning bootstrap cursor")?
                .ok_or_else(|| eyre!("pruning bootstrap cursor row is missing"))?;
            let value = row
                .get::<i64>(0)
                .wrap_err("decode pruning bootstrap cursor")?;
            u64::try_from(value).wrap_err("pruning bootstrap cursor is negative")
        })
    }

    /// A writer for the genesis walk: one connection kept for the whole walk.
    /// Called from a blocking thread, since walking the genesis state is
    /// synchronous provider work.
    #[allow(dead_code)] // the genesis walk went with seeding
    pub(crate) fn genesis_bootstrap_writer_blocking(&self) -> Result<GenesisBootstrapWriter> {
        let connection = self
            .database
            .connect()
            .wrap_err("connect for genesis bootstrap")?;
        Ok(GenesisBootstrapWriter {
            connection,
            runtime: self.runtime.clone(),
        })
    }

    /// Upsert genesis entries and move the walk's cursor to `next_id` as one
    /// transaction, over a writer used once.
    /// [`Self::genesis_bootstrap_writer_blocking`] is for the walk itself.
    #[cfg(test)]
    pub(crate) fn bootstrap_entries_blocking(
        &self,
        entries: &[PruningEntry],
        next_id: u64,
    ) -> Result<()> {
        self.genesis_bootstrap_writer_blocking()?
            .write_blocking(&mut entries.to_vec(), next_id)
    }

    #[allow(dead_code)] // the genesis walk went with seeding
    pub(crate) async fn mark_genesis_bootstrapped(&self) -> Result<()> {
        let connection = self
            .database
            .connect()
            .wrap_err("connect for bootstrap flag")?;
        connection
            .execute(
                "UPDATE pruning_bootstrap SET genesis_done = 1 WHERE singleton = 1",
                (),
            )
            .await
            .wrap_err("record genesis bootstrap")?;
        Ok(())
    }
}

/// The genesis walk's connection; see
/// [`ChainPruningMap::genesis_bootstrap_writer_blocking`].
#[allow(dead_code)] // the genesis walk went with seeding
pub(crate) struct GenesisBootstrapWriter {
    connection: Connection,
    runtime: Handle,
}

impl GenesisBootstrapWriter {
    /// Upsert `entries` and move the walk's cursor to `next_id` as one
    /// transaction, leaving the watermark alone. The entries are sorted by key
    /// first, so the transaction walks the B-tree once, in order.
    ///
    /// Genesis keys are hashes, so a batch touches about as many leaf pages as
    /// it has rows, up to the whole table, and the commit writes every touched
    /// page out again. The cost of a commit is therefore nearly the same for a
    /// batch of a hundred thousand rows as for one of a million: callers
    /// should make them large.
    #[allow(dead_code)] // the genesis walk went with seeding
    pub(crate) fn write_blocking(
        &mut self,
        entries: &mut [PruningEntry],
        next_id: u64,
    ) -> Result<()> {
        entries.sort_unstable_by_key(|entry| entry.key);
        self.runtime.block_on(async {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .await
                .wrap_err("begin genesis bootstrap transaction")?;
            for entry in entries.iter() {
                upsert(&transaction, *entry).await?;
            }
            transaction
                .execute(
                    "UPDATE pruning_bootstrap_cursor SET next_id = ?1 WHERE singleton = 1",
                    turso::params![sql_integer(next_id, "bootstrap cursor")?],
                )
                .await
                .wrap_err("advance genesis bootstrap cursor")?;
            transaction
                .commit()
                .await
                .wrap_err("commit genesis bootstrap batch")
        })
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
                sql_expiry(entry.expires_at),
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

/// An expiry past `i64::MAX` (an entity that never expires is `u64::MAX`) is
/// stored saturated: no chain reaches that height, so the row is kept for
/// later updates but never selected.
fn sql_expiry(expires_at: u64) -> i64 {
    i64::try_from(expires_at).unwrap_or(i64::MAX)
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

    /// An entity that never expires (`u64::MAX`, the seeder's default) is
    /// stored rather than rejected, and is never selected; a later finite
    /// expiry on the same entity still replaces it.
    #[tokio::test]
    async fn a_never_expiring_entry_is_kept_but_never_selected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pruning.db");
        let map = ChainPruningMap::open(&path, Handle::current())
            .await
            .unwrap();
        map.apply_next(1, &[entry(5, u64::MAX, 0)], &[])
            .await
            .unwrap();
        assert!(
            select(map.clone(), 1, (i64::MAX - 1) as u64, 10, u64::MAX)
                .await
                .is_empty()
        );

        map.apply_next(2, &[entry(5, 3, 0)], &[]).await.unwrap();
        assert_eq!(
            select(map.clone(), 2, 3, 10, u64::MAX).await,
            vec![B256::repeat_byte(5)]
        );
    }

    /// Genesis entries land without touching the watermark, and the flag that
    /// stops the walk from repeating survives a reopen.
    #[tokio::test]
    async fn genesis_bootstrap_is_recorded_and_selectable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pruning.db");
        let runtime = Handle::current();
        let map = ChainPruningMap::open(&path, runtime.clone()).await.unwrap();
        assert!(!map.genesis_bootstrapped().await.unwrap());

        let seeded = map.clone();
        tokio::task::spawn_blocking(move || {
            seeded
                .bootstrap_entries_blocking(&[entry(7, 3, 1), entry(8, 9, 1)], 2)
                .unwrap();
        })
        .await
        .unwrap();
        map.mark_genesis_bootstrapped().await.unwrap();
        assert_eq!(map.watermark().await.unwrap(), 0);

        // Selectable at the (unchanged) genesis watermark, expiring in order.
        assert_eq!(
            select(map.clone(), 0, 3, 10, u64::MAX).await,
            vec![B256::repeat_byte(7)]
        );

        drop(map);
        let reopened = ChainPruningMap::open(&path, runtime).await.unwrap();
        assert!(reopened.genesis_bootstrapped().await.unwrap());
        assert_eq!(
            select(reopened, 0, 9, 10, u64::MAX).await,
            vec![B256::repeat_byte(7), B256::repeat_byte(8)]
        );
    }

    /// A walk interrupted part-way (a restart, or reth timing out the read
    /// transaction) resumes where its last committed batch left off: the
    /// cursor travels in the batch's transaction and survives a reopen.
    #[tokio::test]
    async fn genesis_bootstrap_cursor_moves_with_each_batch_and_survives_a_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pruning.db");
        let runtime = Handle::current();
        let map = ChainPruningMap::open(&path, runtime.clone()).await.unwrap();
        let cursor = |map: ChainPruningMap| async move {
            tokio::task::spawn_blocking(move || map.genesis_cursor_blocking().unwrap())
                .await
                .unwrap()
        };
        assert_eq!(cursor(map.clone()).await, 0);

        let seeded = map.clone();
        tokio::task::spawn_blocking(move || {
            seeded
                .bootstrap_entries_blocking(&[entry(1, 5, 0)], 40)
                .unwrap();
            seeded
                .bootstrap_entries_blocking(&[entry(2, 5, 0)], 90)
                .unwrap();
        })
        .await
        .unwrap();
        assert_eq!(cursor(map.clone()).await, 90);
        assert!(!map.genesis_bootstrapped().await.unwrap());

        drop(map);
        let reopened = ChainPruningMap::open(&path, runtime).await.unwrap();
        assert_eq!(cursor(reopened.clone()).await, 90);
        assert_eq!(
            select(reopened, 0, 5, 10, u64::MAX).await,
            vec![B256::repeat_byte(1), B256::repeat_byte(2)]
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

#[cfg(test)]
mod bench {
    use super::*;

    fn entries(from: u64, count: usize) -> Vec<PruningEntry> {
        (from..from + count as u64)
            .map(|i| PruningEntry {
                key: alloy_primitives::keccak256(i.to_be_bytes()),
                expires_at: u64::MAX,
                attribute_count: 3,
            })
            .collect()
    }

    /// `BENCH_PREFILL=4000000 cargo test -p arkiv-reth-payload-builder bench_bootstrap_batch --release -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn bench_bootstrap_batch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pruning.db");
        let map = ChainPruningMap::open(&path, Handle::current())
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || {
            let mut writer = map.genesis_bootstrap_writer_blocking().unwrap();
            // Grow the table first: a fresh table hides the page-in cost.
            let prefill: usize = std::env::var("BENCH_PREFILL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2_000_000);
            let mut rows = entries(1_000_000_000, prefill);
            let started = std::time::Instant::now();
            for chunk in rows.chunks_mut(80_000) {
                writer.write_blocking(chunk, 0).unwrap();
            }
            eprintln!("prefill of {prefill} rows in {:?}", started.elapsed());
            let mut small = entries(0, 80_000);
            let started = std::time::Instant::now();
            writer.write_blocking(&mut small, 80_000).unwrap();
            eprintln!(
                "80k upserts into a {prefill}-row table in {:?}",
                started.elapsed()
            );
            let mut big = entries(2_000_000_000, 1_000_000);
            let started = std::time::Instant::now();
            writer.write_blocking(&mut big, 80_000).unwrap();
            eprintln!("1M upserts into the same table in {:?}", started.elapsed());
        })
        .await
        .unwrap();
    }
}
