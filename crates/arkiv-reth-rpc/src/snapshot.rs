//! Select authenticated records through an Ethereum state snapshot.
use alloy_primitives::B256;
use reth_storage_api::StateProviderBox;

/// Select custom state through the root in this exact Ethereum snapshot.
pub fn authenticated_snapshot(
    state: StateProviderBox,
    records: arkiv_authenticated_store::Store,
) -> eyre::Result<arkiv_authenticated_store::State<arkiv_authenticated_store::Store>> {
    use arkiv_reth_statemanager::authenticated::{ROOT_ACCOUNT, root_slot};
    let root = state
        .storage(ROOT_ACCOUNT, B256::from(root_slot()))?
        .map(B256::from)
        .unwrap_or_default();
    arkiv_authenticated_store::State::open(records, root)
}
