//! The `--chain` value parser producing an [`ArkivChainSpec`].

use crate::ArkivChainSpec;
use alloy_genesis::Genesis;
use reth_cli::chainspec::ChainSpecParser;
use reth_ethereum::chainspec::ChainSpec;
use reth_ethereum_cli::chainspec::chain_value_parser;
use std::path::PathBuf;
use std::sync::Arc;

/// `--chain` values an Arkiv node accepts by name. Anything else is read as a
/// genesis file path or inline genesis JSON, as with reth.
///
/// Only `dev` is named: Arkiv's no-EVM executor cannot follow the public
/// Ethereum networks, so offering their names would only produce a node that
/// rejects every block. `dev` is also the default when nothing is given.
pub const SUPPORTED_CHAINS: &[&str] = &["dev"];

/// Clap value parser for `--chain`: reth's own parser, wrapped in
/// [`ArkivChainSpec`] so the minimum-base-fee rule applies to every chain the
/// node can be pointed at.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ArkivChainSpecParser;

impl ChainSpecParser for ArkivChainSpecParser {
    type ChainSpec = ArkivChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<ArkivChainSpec>> {
        if SUPPORTED_CHAINS.contains(&s) {
            let inner = chain_value_parser(s)?;
            return Ok(Arc::new(ArkivChainSpec::new(
                Arc::try_unwrap(inner).unwrap_or_else(|arc| (*arc).clone()),
            )));
        }
        Ok(Arc::new(ArkivChainSpec::new(parse_genesis_spec(s)?)))
    }
}

/// A genesis file path or inline genesis JSON, as reth reads it.
fn parse_genesis_spec(s: &str) -> eyre::Result<ChainSpec> {
    // reth's rule: a readable path first, else the text itself if it looks
    // like JSON.
    let raw = match std::fs::read_to_string(PathBuf::from(s)) {
        Ok(raw) => raw,
        Err(io_err) => {
            if s.contains('{') {
                s.to_string()
            } else {
                return Err(io_err.into());
            }
        }
    };
    let genesis: Genesis = serde_json::from_str(&raw)?;
    Ok(ChainSpec::from_genesis(genesis))
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
}
