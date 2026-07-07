//! [`SnapshotAccountCode`] — the reth **read-path** bridge.
//!
//! The mirror of the executor's write-path `ExecutorState`: it implements the
//! entity store's [`AccountCode`] seam over a committed reth state snapshot
//! ([`StateProviderBox`]), so `RethEntityStore<CodeBackend<SnapshotAccountCode>>`
//! can read entities back for RPC. Reads only — the write half never runs here.

use alloy_primitives::Address;
use arkiv_reth_entitystore::AccountCode;
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
