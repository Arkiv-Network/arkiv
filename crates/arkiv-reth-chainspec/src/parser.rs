//! The `--chain` value parser producing an [`ArkivChainSpec`].

use crate::ArkivChainSpec;
use alloy_genesis::Genesis;
use alloy_primitives::B256;
use reth_cli::chainspec::ChainSpecParser;
use reth_ethereum::chainspec::ChainSpec;
use reth_ethereum::primitives::SealedHeader;
use reth_ethereum_cli::chainspec::chain_value_parser;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

/// `--chain` values an Arkiv node accepts by name. Anything else is read as a
/// genesis file path or inline genesis JSON, as with reth.
///
/// Only `dev` is named: Arkiv's no-EVM executor cannot follow the public
/// Ethereum networks, so offering their names would only produce a node that
/// rejects every block. `dev` is also the default when nothing is given.
pub const SUPPORTED_CHAINS: &[&str] = &["dev"];

/// The genesis field naming the block-0 state root outright, for a genesis
/// whose state is imported with `init-state` rather than listed in `alloc`.
/// geth's spelling, with geth's rule: it and a non-empty `alloc` are mutually
/// exclusive, because the root is otherwise derived from the alloc.
pub const STATE_HASH_FIELD: &str = "stateHash";

/// Clap value parser for `--chain`: reth's own parser, wrapped in
/// [`ArkivChainSpec`] so the minimum-base-fee rule applies to every chain the
/// node can be pointed at — plus [`STATE_HASH_FIELD`] on a genesis file.
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

/// The one genesis field read past the `Genesis` type: [`STATE_HASH_FIELD`].
#[derive(Deserialize)]
struct StateHash {
    #[serde(rename = "stateHash")]
    state_hash: Option<B256>,
}

/// A genesis file path or inline genesis JSON, as reth reads it, honouring
/// [`STATE_HASH_FIELD`].
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
    // Straight into `Genesis`, as reth does, then a second pass for the one
    // field that type does not carry. The second pass allocates nothing per
    // account; going through a `serde_json::Value` instead would cost several
    // times the alloc's size in memory, on a file that can run to gigabytes.
    let genesis: Genesis = serde_json::from_str(&raw)?;
    let StateHash { state_hash } = serde_json::from_str(&raw)?;
    let mut spec = ChainSpec::from_genesis(genesis);
    if let Some(root) = state_hash {
        eyre::ensure!(
            spec.genesis.alloc.is_empty(),
            "genesis carries both {STATE_HASH_FIELD} and a non-empty alloc; the state root is \
             derived from the alloc, so it can name one or the other"
        );
        let mut header = spec.genesis_header.clone_header();
        header.state_root = root;
        spec.genesis_header = SealedHeader::seal_slow(header);
    }
    Ok(spec)
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

    /// `stateHash` replaces the alloc-derived root in the genesis header, and
    /// with it the genesis hash — the header an `init-state` import is checked
    /// against.
    #[test]
    fn state_hash_sets_the_genesis_state_root() {
        let root = B256::repeat_byte(0x42);
        let mut value: serde_json::Value = serde_json::from_str(GENESIS).unwrap();
        value[STATE_HASH_FIELD] = serde_json::to_value(root).unwrap();
        let seeded = ArkivChainSpecParser::parse(&value.to_string()).expect("parses");
        let plain = ArkivChainSpecParser::parse(GENESIS).unwrap();

        assert_eq!(seeded.genesis_header().state_root, root);
        assert_ne!(plain.genesis_header().state_root, root);
        assert_ne!(seeded.genesis_hash(), plain.genesis_hash());
        assert_eq!(
            seeded.genesis_hash(),
            alloy_primitives::keccak256(alloy_rlp::encode(seeded.genesis_header())),
            "the sealed hash is the header's own hash",
        );
        assert!(seeded.genesis().alloc.is_empty());
        // Everything else is untouched.
        assert_eq!(seeded.min_base_fee(), 10);
        assert_eq!(seeded.chain().id(), 7738577);
    }

    #[test]
    fn state_hash_with_an_alloc_is_refused() {
        let mut value: serde_json::Value = serde_json::from_str(GENESIS).unwrap();
        value[STATE_HASH_FIELD] = serde_json::to_value(B256::repeat_byte(1)).unwrap();
        value["alloc"] = serde_json::json!({
            "0x0000000000000000000000000000000000000001": {"balance": "0x1"}
        });
        let err = ArkivChainSpecParser::parse(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains(STATE_HASH_FIELD), "{err}");
    }
}
