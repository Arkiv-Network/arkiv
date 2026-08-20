//! [`SnapshotAccountCode`] — the reth **read-path** bridge.
//!
//! The mirror of the write path's `WriteOverlay` (arkiv-reth-statemanager): it implements both the
//! entity store's [`AccountCode`] seam (entity bytes + tier-1 pair bitmaps live in
//! account code) and the query index's [`IndexStorage`] seam (tier-2 range
//! structures + the id maps live in storage slots) over a committed reth state
//! snapshot ([`StateProviderBox`]). That lets `RethEntityStore<CodeBackend<_>>` read
//! entities *and* `RethAuxStore<_>` evaluate queries off the same snapshot. Reads
//! only — the write halves (`set_*`) never run here.

use alloy_primitives::{Address, B256};
use arkiv_reth_mpt_committed_store::AccountCode;
use arkiv_reth_mpt_committed_store::IndexStorage;
use reth_storage_api::StateProviderBox;

/// [`AccountCode`] over a read-only reth state snapshot.
pub struct SnapshotAccountCode {
    state: StateProviderBox,
}

impl SnapshotAccountCode {
    /// Read entities as of the state in `state` (typically the latest tip).
    pub fn new(state: StateProviderBox) -> Self {
        Self { state }
    }
}

impl AccountCode for SnapshotAccountCode {
    type Error = eyre::Report;

    fn code(&mut self, address: Address) -> Result<Vec<u8>, Self::Error> {
        let code = self
            .state
            .account_code(&address)
            .map_err(|e| eyre::eyre!("account_code({address}): {e:?}"))?;
        Ok(code
            .map(|c| c.original_bytes().to_vec())
            .unwrap_or_default())
    }

    fn set_code(&mut self, _address: Address, _code: Vec<u8>) -> Result<(), Self::Error> {
        eyre::bail!("read-only snapshot: set_code is unsupported")
    }

    fn clear_code(&mut self, _address: Address) -> Result<(), Self::Error> {
        eyre::bail!("read-only snapshot: clear_code is unsupported")
    }
}

impl IndexStorage for SnapshotAccountCode {
    type Error = eyre::Report;

    fn storage(&mut self, address: Address, slot: B256) -> Result<B256, Self::Error> {
        // Absent slot → `None` → `B256::ZERO`, matching the index's "absent reads as
        // zero" convention.
        let value = self
            .state
            .storage(address, slot)
            .map_err(|e| eyre::eyre!("storage({address}, {slot}): {e:?}"))?;
        Ok(value.map(B256::from).unwrap_or(B256::ZERO))
    }

    fn set_storage(
        &mut self,
        _address: Address,
        _slot: B256,
        _value: B256,
    ) -> Result<(), Self::Error> {
        eyre::bail!("read-only snapshot: set_storage is unsupported")
    }

    fn ensure_account_persists(&mut self, _address: Address) -> Result<(), Self::Error> {
        eyre::bail!("read-only snapshot: ensure_account_persists is unsupported")
    }
}
