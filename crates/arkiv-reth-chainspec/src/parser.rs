//! The `--chain` value parser producing an [`ArkivChainSpec`].

use crate::ArkivChainSpec;
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};
use std::sync::Arc;

/// `--chain` values an Arkiv node accepts by name. Anything else is read as a
/// genesis file path or inline genesis JSON, as with reth.
///
/// Only `dev` is named: Arkiv's no-EVM executor cannot follow the public
/// Ethereum networks, so offering their names would only produce a node that
/// rejects every block. `dev` is also the default when nothing is given.
pub const SUPPORTED_CHAINS: &[&str] = &["dev"];

/// The `dev` chain: reth's dev chain (chain ID 1337, every fork from block 0,
/// its funded dev accounts) with GolemDB's settings.
const DEV_GENESIS: &str = include_str!("../../../genesis/dev.json");

/// Clap value parser for `--chain`: `dev` from `genesis/dev.json`, anything
/// else through reth's genesis reader, wrapped in [`ArkivChainSpec`] so the
/// minimum-base-fee rule applies to every chain the node can be pointed at.
///
/// Its block 0 is reth's, built from `alloc`. The node reads `--chain` with a
/// parser on top of this one that sets block 0's `stateRoot` to GolemDB's
/// genesis root (`bin/arkiv-reth/src/genesis.rs`), so that one gives the
/// node's genesis hash.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ArkivChainSpecParser;

impl ChainSpecParser for ArkivChainSpecParser {
    type ChainSpec = ArkivChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<ArkivChainSpec>> {
        let genesis = if s == "dev" {
            serde_json::from_str(DEV_GENESIS)?
        } else {
            parse_genesis(s)?
        };
        Ok(Arc::new(ArkivChainSpec::from_genesis(genesis)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip1559::INITIAL_BASE_FEE;
    use reth_ethereum::chainspec::EthChainSpec;

    const GENESIS: &str = r#"{
        "config": {"chainId": 7738577, "londonBlock": 0, "terminalTotalDifficulty": 0,
                   "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0},
        "gasLimit": "0x3938700", "difficulty": "0x0", "baseFeePerGas": "0xa", "alloc": {}
    }"#;

    #[test]
    fn named_chains_parse() {
        for &chain in SUPPORTED_CHAINS {
            let spec = ArkivChainSpecParser::parse(chain).expect("named chain parses");
            assert_eq!(spec.min_base_fee(), INITIAL_BASE_FEE);
        }
    }

    /// As this parser reads it, `dev` is reth's dev chain, block 0 included,
    /// with GolemDB's settings.
    #[test]
    fn the_dev_chain_is_reths_with_golemdb_settings() {
        use reth_ethereum::chainspec::{DEV, ForkCondition, Hardfork, Hardforks};
        let spec = ArkivChainSpecParser::parse("dev").unwrap();
        assert_eq!(spec.chain(), DEV.chain());
        assert_eq!(spec.genesis_hash(), DEV.genesis_hash());
        assert_eq!(
            spec.final_paris_total_difficulty(),
            DEV.final_paris_total_difficulty()
        );
        // The same forks, each active at genesis or not as on reth's.
        let active = |forks: Vec<(&dyn Hardfork, ForkCondition)>| {
            forks
                .into_iter()
                .map(|(fork, condition)| {
                    (fork.name(), condition.active_at_timestamp_or_number(0, 0))
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            active(spec.forks_iter().collect()),
            active(DEV.forks_iter().collect())
        );
        assert!(spec.golemdb().is_some());
    }

    #[test]
    fn genesis_json_parses_with_its_base_fee_as_the_floor() {
        let spec = ArkivChainSpecParser::parse(GENESIS).expect("inline genesis parses");
        assert_eq!(spec.min_base_fee(), 10);
        assert_eq!(spec.chain().id(), 7738577);
    }

    #[test]
    fn genesis_file_parses_like_inline_json() {
        let path = std::env::temp_dir().join(format!(
            "arkiv-chainspec-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, GENESIS).unwrap();
        let from_file = ArkivChainSpecParser::parse(path.to_str().unwrap()).expect("file parses");
        let _ = std::fs::remove_file(&path);
        let inline = ArkivChainSpecParser::parse(GENESIS).unwrap();
        assert_eq!(from_file.genesis_hash(), inline.genesis_hash());
    }

    #[test]
    fn a_bad_section_fails_the_parse() {
        let mut genesis: serde_json::Value = serde_json::from_str(GENESIS).unwrap();
        genesis["config"]["arkiv"] = serde_json::json!({ "golemdb": { "hash_fn": "keccak-256" } });
        let error = ArkivChainSpecParser::parse(&genesis.to_string()).unwrap_err();
        assert!(
            error.to_string().contains("unknown field `hash_fn`"),
            "{error}"
        );
    }
}
