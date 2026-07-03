//! [`ReadOnlyStateAdapter`] — bridges reth's [`StateProvider`] + a DB
//! cursor provider to [`arkiv_db_engine::StateAdapter`] for query execution.
//!
//! Single-key reads (code, storage) go through `StateProviderBox`.
//! [`StateAdapter::iter_storage_asc`] opens a duplicate-sort cursor on the
//! `PlainStorageState` MDBX table for efficient range iteration.
//!
//! **Limitation**: `iter_storage_asc` reads from the *current* plain-state
//! table regardless of the snapshot requested by the caller. Range queries
//! against historical blocks may therefore return stale index data. Point
//! reads (code, storage) ARE historical via the `StateProviderBox`.

use alloy_primitives::{Address, B256};
use arkiv_db_engine::StateAdapter;
use eyre::Result;
use reth_db_api::{
    cursor::DbDupCursorRO,
    tables,
    transaction::DbTx,
};
use reth_storage_api::{DBProvider, StateProviderBox};

pub struct ReadOnlyStateAdapter<DP> {
    state: StateProviderBox,
    db_provider: DP,
}

impl<DP: DBProvider> ReadOnlyStateAdapter<DP>
where
    DP::Tx: DbTx,
{
    pub fn new(state: StateProviderBox, db_provider: DP) -> Self {
        Self { state, db_provider }
    }
}

impl<DP: DBProvider> StateAdapter for ReadOnlyStateAdapter<DP>
where
    DP::Tx: DbTx,
{
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
        let maybe = self
            .state
            .account_code(addr)
            .map_err(|e| eyre::eyre!("account_code({addr}): {e:?}"))?;
        Ok(maybe.map(|b| b.original_bytes().to_vec()).unwrap_or_default())
    }

    fn set_code(&mut self, _addr: &Address, _code: Vec<u8>) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_code is not supported")
    }

    fn tombstone_code(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: tombstone_code is not supported")
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        let val = self
            .state
            .storage(*addr, slot)
            .map_err(|e| eyre::eyre!("storage({addr}, {slot}): {e:?}"))?
            .unwrap_or_default();
        Ok(B256::from(val.to_be_bytes::<32>()))
    }

    fn set_storage(&mut self, _addr: &Address, _slot: B256, _value: B256) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_storage is not supported")
    }

    fn ensure_account_persists(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: ensure_account_persists is not supported")
    }

    fn iter_storage_asc(&mut self, addr: &Address, from: B256) -> Result<Vec<(B256, B256)>> {
        let mut cursor = self
            .db_provider
            .tx_ref()
            .cursor_dup_read::<tables::PlainStorageState>()
            .map_err(|e| eyre::eyre!("cursor_dup_read: {e:?}"))?;

        let walker = cursor
            .walk_dup(Some(*addr), Some(from))
            .map_err(|e| eyre::eyre!("walk_dup: {e:?}"))?;

        walker
            .map(|r| {
                let (_, entry) = r.map_err(|e| eyre::eyre!("cursor iter: {e:?}"))?;
                Ok((entry.key, B256::from(entry.value.to_be_bytes::<32>())))
            })
            .collect()
    }
}
