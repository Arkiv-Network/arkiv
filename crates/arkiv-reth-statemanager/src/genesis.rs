//! Portable custom state carried in a chain's genesis configuration.
use crate::authenticated::{ROOT_ACCOUNT, root_slot};
use alloy_genesis::Genesis;
use alloy_primitives::B256;
use arkiv_authenticated_store::{EMPTY_ROOT, Snapshot, Store};

pub const SNAPSHOT_FIELD: &str = "arkivState";

pub fn with_snapshot(mut genesis: Genesis, snapshot: &Snapshot) -> eyre::Result<Genesis> {
    genesis
        .config
        .extra_fields
        .insert(SNAPSHOT_FIELD.into(), serde_json::to_value(snapshot)?);
    Ok(genesis)
}

/// Seed records before any execution/RPC starts. The state root authenticates
/// the snapshot through ROOT_ACCOUNT; an absent snapshot cannot hide missing data.
pub fn initialize(genesis: &Genesis, store: Store) -> eyre::Result<()> {
    let root = genesis
        .alloc
        .get(&ROOT_ACCOUNT)
        .and_then(|a| a.storage.as_ref())
        .and_then(|s| s.get(&B256::from(root_slot())))
        .copied()
        .unwrap_or(EMPTY_ROOT);
    if let Some(value) = genesis.config.extra_fields.get(SNAPSHOT_FIELD) {
        let snapshot: Snapshot = serde_json::from_value(value.clone())?;
        // stateHash genesis carries an external native alloc; its root is checked
        // by init-state. Startup checks its actual system slot separately.
        eyre::ensure!(
            genesis.alloc.is_empty() || snapshot.root == root,
            "genesis Arkiv snapshot does not match the system account root"
        );
        snapshot.import(store)?;
    } else {
        eyre::ensure!(root == EMPTY_ROOT, "genesis is missing config.arkivState");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::GenesisAccount;
    use arkiv_authenticated_store::State;
    use arkiv_interfaces::entity::Entity;

    #[test]
    fn genesis_records_must_match_the_committed_root() -> eyre::Result<()> {
        let mut state = State::open(Store::default(), EMPTY_ROOT)?;
        state.put_entity(&Entity {
            key: [1; 32],
            expires_at: 100,
            ..Default::default()
        })?;
        let snapshot = state.snapshot()?;
        let mut genesis = Genesis::default();
        genesis.alloc.insert(
            ROOT_ACCOUNT,
            GenesisAccount {
                nonce: Some(1),
                storage: Some([(B256::from(root_slot()), snapshot.root)].into()),
                ..Default::default()
            },
        );
        assert!(initialize(&genesis, Store::default()).is_err());
        let mut genesis = with_snapshot(genesis, &snapshot)?;
        let records = Store::default();
        initialize(&genesis, records.clone())?;
        assert!(
            State::open(records, snapshot.root)?
                .entity([1; 32])?
                .is_some()
        );
        genesis
            .alloc
            .get_mut(&ROOT_ACCOUNT)
            .unwrap()
            .storage
            .as_mut()
            .unwrap()
            .insert(B256::from(root_slot()), B256::repeat_byte(5));
        assert!(initialize(&genesis, Store::default()).is_err());
        Ok(())
    }
}
