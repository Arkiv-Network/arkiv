//! [`ReadOnlyStateAdapter`] — bridges reth's [`StateProvider`] to
//! [`arkiv_db_engine::StateAdapter`] for query execution.
//!
//! All reads — single-key (`code`, `storage`) and range (`iter_storage_asc`)
//! — go through a single `StateProviderBox` snapshot, so they observe a
//! consistent view (including reth's in-memory canonical state for `latest`).
//!
//! Range iteration cannot use an MDBX cursor: the entity engine addresses
//! its Tier-2 index nodes by `keccak`-derived account addresses, so index
//! entries are scattered across the keyspace rather than laid out in
//! sorted-slot order under one account. `iter_storage_asc` therefore
//! delegates to [`arkiv_db_engine::iter_storage_asc_impl`], which walks the
//! B+tree / list structure with point `storage` reads against the snapshot.

use alloy_primitives::{Address, B256};
use arkiv_db_engine::{StateAdapter, iter_storage_asc_impl};
use eyre::Result;
use reth_storage_api::StateProviderBox;

pub struct ReadOnlyStateAdapter {
    state: StateProviderBox,
}

impl ReadOnlyStateAdapter {
    pub fn new(state: StateProviderBox) -> Self {
        Self { state }
    }
}

impl StateAdapter for ReadOnlyStateAdapter {
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
        // Structural walk over point reads against the snapshot — see module docs.
        iter_storage_asc_impl(self, addr, from)
    }
}
