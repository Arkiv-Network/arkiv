//! The chain the node runs: `--chain` as [`ArkivChainSpecParser`] reads it,
//! with block 0's `stateRoot` set to GolemDB's root after the genesis accounts
//! are written, when the genesis file sets GolemDB's settings. The genesis hash
//! then covers those settings and the starting balances.
//!
//! The root is computed in memory with the code that seeds the store, before
//! any store opens: reth needs the genesis hash as soon as it reads `--chain`.

use std::sync::Arc;

use alloy_primitives::B256;
use arkiv_reth_chainspec::{ArkivChainSpec, ArkivChainSpecParser};
use arkiv_reth_statemanager::{GenesisAccount, HostStore, seed_genesis};
use reth_cli::chainspec::ChainSpecParser;
use reth_ethereum::chainspec::EthChainSpec;

/// The `--chain` value parser the node runs with (see the module docs).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub(crate) struct NodeChainSpecParser;

impl ChainSpecParser for NodeChainSpecParser {
    type ChainSpec = ArkivChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = ArkivChainSpecParser::SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<ArkivChainSpec>> {
        let spec = ArkivChainSpecParser::parse(s)?;
        if spec.golemdb().is_none() {
            return Ok(spec);
        }
        let root = genesis_root(&spec)?;
        Ok(Arc::new(
            Arc::unwrap_or_clone(spec).with_genesis_state_root(root),
        ))
    }
}

/// GolemDB's state root at the store's genesis commit, the one holding the
/// genesis accounts.
fn genesis_root(spec: &ArkivChainSpec) -> eyre::Result<B256> {
    let store = arkiv_golemdb::GolemDbStore::open_memory(&super::golemdb_config(spec)?)
        .map_err(|e| eyre::eyre!("compute GolemDB's genesis root: {e}"))?;
    let store: HostStore = Arc::new(store);
    let accounts = GenesisAccount::from_alloc(&spec.genesis().alloc);
    // The commit's tag sits outside the state root, so any tag gives the same
    // root.
    let commit = seed_genesis(&store, [0; 32], accounts)
        .map_err(|e| eyre::eyre!("compute GolemDB's genesis root: {e:?}"))?
        .ok_or_else(|| eyre::eyre!("compute GolemDB's genesis root: the store has a genesis"))?;
    let root = store
        .commit_hash(commit)
        .map_err(|e| eyre::eyre!("compute GolemDB's genesis root: {e:?}"))?;
    Ok(B256::from(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const EXAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../genesis/example.json");

    /// The example genesis file, then `edit`ed, as the node's spec and as
    /// [`ArkivChainSpecParser`] alone reads it.
    fn specs(edit: impl FnOnce(&mut Value)) -> (Arc<ArkivChainSpec>, Arc<ArkivChainSpec>) {
        let mut genesis: Value =
            serde_json::from_str(&std::fs::read_to_string(EXAMPLE).unwrap()).unwrap();
        edit(&mut genesis);
        let text = genesis.to_string();
        (
            NodeChainSpecParser::parse(&text).unwrap(),
            ArkivChainSpecParser::parse(&text).unwrap(),
        )
    }

    #[test]
    fn block_0_carries_golemdbs_genesis_root() {
        let (node, file) = specs(|_| {});
        let root = node.genesis_header().state_root;
        assert_eq!(root, genesis_root(&file).unwrap());
        assert_ne!(
            root,
            file.genesis_header().state_root,
            "not reth's own root"
        );
        assert_eq!(
            node.genesis_hash(),
            specs(|_| {}).0.genesis_hash(),
            "the same file, the same genesis"
        );
    }

    #[test]
    fn the_genesis_hash_covers_golemdbs_settings_and_the_balances() {
        let genesis = specs(|_| {}).0.genesis_hash();
        let blake3 = specs(|g| g["config"]["arkiv"]["golemdb"]["hash_function"] = "blake3".into());
        assert_ne!(blake3.0.genesis_hash(), genesis, "hash function");
        let richer = specs(|g| {
            g["alloc"]["0x2222222222222222222222222222222222222222"]["balance"] = "0x2".into();
        });
        assert_ne!(richer.0.genesis_hash(), genesis, "balance");
    }

    /// Every fee-schedule value is part of block 0: two chains that differ in
    /// only one of them have different genesis blocks.
    #[test]
    fn the_genesis_hash_covers_every_pricing_weight() {
        let genesis = specs(|_| {}).0.genesis_hash();
        let weights = specs(|_| {}).1.golemdb().unwrap().metering.weights.clone();
        for name in weights.keys() {
            let changed = specs(|g| {
                let value = &mut g["config"]["arkiv"]["golemdb"]["metering"]["weights"][name];
                *value = (value.as_u64().unwrap() + 1).into();
            });
            assert_ne!(changed.0.genesis_hash(), genesis, "{name}");
        }
    }

    /// A file for an experimental store, without GolemDB's settings, keeps
    /// reth's own block 0.
    #[test]
    fn without_golemdb_settings_block_0_is_reths() {
        let (node, file) = specs(|g| {
            g["config"].as_object_mut().unwrap().remove("arkiv");
        });
        assert_eq!(node.genesis_hash(), file.genesis_hash());
    }
}
