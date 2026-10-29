//! The Arkiv chain specification.
//!
//! [`ArkivChainSpec`] is reth's stock [`ChainSpec`] with one protocol rule on
//! top: the base fee never drops below the genesis block's base fee.
//!
//! ```text
//! next_base_fee = max(eip1559(parent), genesis.baseFeePerGas)
//! ```
//!
//! Every base-fee consumer in reth (payload builder, consensus, transaction pool
//! and the `eth_*` RPC) derives it through [`EthChainSpec::next_block_base_fee`],
//! so overriding that one method is enough; everything else delegates to the
//! wrapped spec. The floor is read from the genesis header and is 0 when that
//! header carries no base fee, which makes the rule inert.
//!
//! This is a consensus rule: a node without it rejects every block produced
//! under it, so every node on the chain must run it.
//!
//! The spec also carries GolemDB's settings for the chain, from the genesis
//! file's `config.arkiv.golemdb` (see [`ArkivChainSpec::golemdb`]).

mod parser;

pub use parser::ArkivChainSpecParser;
pub use reth_ethereum::chainspec::ChainSpec;

use alloy_consensus::Header;
use alloy_eips::{eip1559::BaseFeeParams, eip7840::BlobParams};
use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use core::fmt::Display;
use reth_ethereum::chainspec::{
    Chain, DepositContract, EthChainSpec, EthereumHardfork, EthereumHardforks, ForkCondition,
    ForkFilter, ForkId, Hardfork, Hardforks, Head,
};
use reth_ethereum::primitives::SealedHeader;
use reth_network_peers::NodeRecord;
use serde::Deserialize;

/// reth's [`ChainSpec`] plus the Arkiv minimum-base-fee rule and GolemDB's
/// settings (see the crate docs).
#[derive(Debug, Clone)]
pub struct ArkivChainSpec {
    inner: ChainSpec,
    /// The genesis header's base fee, cached: the floor every later block's base
    /// fee is clamped to. Zero when the genesis header carries no base fee.
    min_base_fee: u64,
    /// The genesis file's `config.arkiv.golemdb`, if it has one.
    golemdb: Option<golemdb_api::Genesis>,
}

/// `config.arkiv` in the genesis file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, expecting = "an object with a `golemdb` section")]
struct ArkivConfig {
    golemdb: golemdb_api::Genesis,
}

impl ArkivChainSpec {
    /// Wrap a reth chain spec. The floor is read from its genesis header, and
    /// GolemDB's settings from its genesis `config.arkiv`, which must be valid.
    pub fn new(inner: ChainSpec) -> eyre::Result<Self> {
        let min_base_fee = inner.initial_base_fee().unwrap_or_default();
        let golemdb = inner
            .genesis()
            .config
            .extra_fields
            .get_deserialized::<ArkivConfig>("arkiv")
            .transpose()
            .map_err(|e| eyre::eyre!("genesis config.arkiv: {e}"))?
            .map(|config| config.golemdb);
        Ok(Self {
            inner,
            min_base_fee,
            golemdb,
        })
    }

    /// Build from a geth-format genesis, the way `--chain <file>` does.
    pub fn from_genesis(genesis: Genesis) -> eyre::Result<Self> {
        Self::new(ChainSpec::from_genesis(genesis))
    }

    /// The wrapped reth chain spec.
    pub fn inner(&self) -> &ChainSpec {
        &self.inner
    }

    /// The base-fee floor: the genesis block's base fee (0 if it has none).
    pub fn min_base_fee(&self) -> u64 {
        self.min_base_fee
    }

    /// The same chain with block 0's `stateRoot` set to `root`, so that the
    /// genesis hash covers it.
    pub fn with_genesis_state_root(mut self, root: B256) -> Self {
        let mut header = self.inner.genesis_header.header().clone();
        header.state_root = root;
        self.inner.genesis_header = SealedHeader::seal_slow(header);
        self
    }

    /// GolemDB's settings from the genesis file's `config.arkiv.golemdb`, in
    /// GolemDB's own genesis format, or `None` if it has none.
    /// `genesis/example.json` in the repository shows the format.
    pub fn golemdb(&self) -> Option<&golemdb_api::Genesis> {
        self.golemdb.as_ref()
    }
}

impl Hardforks for ArkivChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.inner.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
    }
}

impl EthereumHardforks for ArkivChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.ethereum_fork_activation(fork)
    }
}

impl EthExecutorSpec for ArkivChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.inner.deposit_contract_address()
    }
}

impl EthChainSpec for ArkivChainSpec {
    type Header = Header;

    fn chain(&self) -> Chain {
        self.inner.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.inner.blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.inner.deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn Display> {
        Box::new(self.inner.display_hardforks())
    }

    fn genesis_header(&self) -> &Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.inner.bootnodes()
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.inner.final_paris_total_difficulty()
    }

    /// The Arkiv rule: EIP-1559's next base fee, floored at the genesis base fee.
    ///
    /// This is the one override; see the crate docs for who calls it.
    fn next_block_base_fee(&self, parent: &Header, target_timestamp: u64) -> Option<u64> {
        let derived = self.inner.next_block_base_fee(parent, target_timestamp)?;
        Some(derived.max(self.min_base_fee))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip1559::{INITIAL_BASE_FEE, MIN_PROTOCOL_BASE_FEE};
    use golemdb_api::{CellLimits, HashAlgorithm, PricingConfig, RecordLimits};
    use reth_ethereum::chainspec::DEV;
    use serde_json::{Value, json};

    /// A genesis with EIP-1559 active from block 0, with or without an explicit
    /// base fee.
    fn genesis(base_fee: Option<u128>) -> Genesis {
        let mut genesis: Genesis = serde_json::from_value(serde_json::json!({
            "config": {
                "chainId": 7738577,
                "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
                "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
                "mergeNetsplitBlock": 0, "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
            },
            "gasLimit": "0x3938700",
            "difficulty": "0x0",
            "alloc": {}
        }))
        .expect("genesis json");
        genesis.base_fee_per_gas = base_fee;
        genesis
    }

    /// A parent header at the floor that used no gas: stock EIP-1559 would
    /// decrease the next base fee by 1/8.
    fn empty_parent(base_fee: u64, gas_limit: u64) -> Header {
        Header {
            number: 1,
            gas_limit,
            gas_used: 0,
            base_fee_per_gas: Some(base_fee),
            ..Default::default()
        }
    }

    #[test]
    fn floor_is_the_genesis_base_fee() {
        assert_eq!(
            ArkivChainSpec::from_genesis(genesis(Some(10)))
                .unwrap()
                .min_base_fee(),
            10
        );
        assert_eq!(
            ArkivChainSpec::from_genesis(genesis(None))
                .unwrap()
                .min_base_fee(),
            INITIAL_BASE_FEE,
            "no baseFeePerGas in genesis: reth's 1 gwei default is the floor",
        );
        assert_eq!(
            ArkivChainSpec::new((**DEV).clone()).unwrap().min_base_fee(),
            INITIAL_BASE_FEE,
            "--dev floors at reth's dev-genesis base fee",
        );
    }

    #[test]
    fn empty_block_holds_the_floor_where_reth_would_decay() {
        let spec = ArkivChainSpec::from_genesis(genesis(Some(10))).unwrap();
        let parent = empty_parent(10, 60_000_000);
        assert_eq!(
            spec.inner().next_block_base_fee(&parent, 0),
            Some(9),
            "stock reth decays 10 -> 9 after an empty block (and on to 7)",
        );
        assert_eq!(spec.next_block_base_fee(&parent, 0), Some(10));

        // Stock reth's terminal value is the protocol minimum; Arkiv's is the floor.
        let mut stock = 10;
        let mut arkiv = 10;
        for _ in 0..8 {
            stock = spec
                .inner()
                .next_block_base_fee(&empty_parent(stock, 60_000_000), 0)
                .unwrap();
            arkiv = spec
                .next_block_base_fee(&empty_parent(arkiv, 60_000_000), 0)
                .unwrap();
        }
        assert_eq!(stock, MIN_PROTOCOL_BASE_FEE);
        assert_eq!(arkiv, 10);
    }

    #[test]
    fn decay_stops_at_the_floor_not_below() {
        let spec = ArkivChainSpec::from_genesis(genesis(None)).unwrap();
        // 12.5% above the floor: one empty block lands exactly on the floor.
        let parent = empty_parent(INITIAL_BASE_FEE + INITIAL_BASE_FEE / 8, 30_000_000);
        assert_eq!(spec.next_block_base_fee(&parent, 0), Some(INITIAL_BASE_FEE));
        // Already on the floor: stays there.
        let parent = empty_parent(INITIAL_BASE_FEE, 30_000_000);
        assert_eq!(spec.next_block_base_fee(&parent, 0), Some(INITIAL_BASE_FEE));
    }

    #[test]
    fn above_the_floor_is_plain_eip1559() {
        let spec = ArkivChainSpec::from_genesis(genesis(Some(10))).unwrap();
        // A full block raises the fee by 1/8; a well-above-floor empty block
        // decays by 1/8 — both exactly as reth computes them.
        for parent in [
            Header {
                number: 5,
                gas_limit: 30_000_000,
                gas_used: 30_000_000,
                base_fee_per_gas: Some(1_000),
                ..Default::default()
            },
            empty_parent(1_000, 30_000_000),
        ] {
            assert_eq!(
                spec.next_block_base_fee(&parent, 0),
                spec.inner().next_block_base_fee(&parent, 0),
            );
        }
    }

    #[test]
    fn no_genesis_base_fee_means_no_floor() {
        // EIP-1559 not active at block 0: the genesis header has no base fee.
        let mut genesis = genesis(None);
        genesis.config.london_block = Some(100);
        let spec = ArkivChainSpec::from_genesis(genesis).unwrap();
        assert_eq!(spec.min_base_fee(), 0);
        assert_eq!(spec.genesis_header().base_fee_per_gas, None);
    }

    /// A new block 0 `stateRoot` is a new genesis header and hash; nothing
    /// else changes.
    #[test]
    fn a_genesis_state_root_moves_the_genesis_hash() {
        let spec = ArkivChainSpec::from_genesis(genesis(Some(10))).unwrap();
        let root = B256::repeat_byte(7);
        let moved = spec.clone().with_genesis_state_root(root);
        assert_eq!(moved.genesis_header().state_root, root);
        assert_eq!(moved.genesis_hash(), moved.genesis_header().hash_slow());
        assert_ne!(moved.genesis_hash(), spec.genesis_hash());
        let mut header = moved.genesis_header().clone();
        header.state_root = spec.genesis_header().state_root;
        assert_eq!(&header, spec.genesis_header());
        assert_eq!(moved.min_base_fee(), spec.min_base_fee());
    }

    /// `config.arkiv` with GolemDB's settings, then `edit`ed.
    fn arkiv(edit: impl FnOnce(&mut Value)) -> Value {
        let mut golemdb = json!({
            "hash_function": "blake3",
            "cell_limits": { "max_cell_name_len": 32, "max_str_len": 128, "max_bytes_len": 131072 },
            "record_limits": { "max_cells": 39, "max_indexed_cells": 36 },
            "metering": serde_json::to_value(PricingConfig::zero()).expect("pricing json")
        });
        edit(&mut golemdb);
        json!({ "golemdb": golemdb })
    }

    /// The chain spec of a genesis whose `config.arkiv` is `arkiv`.
    fn with_arkiv(arkiv: Value) -> eyre::Result<ArkivChainSpec> {
        let mut genesis = genesis(None);
        genesis.config.extra_fields.insert("arkiv".into(), arkiv);
        ArkivChainSpec::from_genesis(genesis)
    }

    #[test]
    fn golemdb_settings_come_from_config_arkiv() {
        assert_eq!(
            ArkivChainSpec::new((**DEV).clone()).unwrap().golemdb(),
            None
        );
        assert_eq!(
            ArkivChainSpec::from_genesis(genesis(None))
                .unwrap()
                .golemdb(),
            None
        );
        let limits = CellLimits {
            max_cell_name_len: 32,
            max_str_len: 128,
            max_bytes_len: 131072,
        };
        let record_limits = RecordLimits {
            max_cells: 39,
            max_indexed_cells: 36,
        };
        assert_eq!(
            with_arkiv(arkiv(|_| {})).unwrap().golemdb(),
            Some(&golemdb_api::Genesis::new(
                HashAlgorithm::Blake3,
                limits,
                record_limits,
                None,
                PricingConfig::zero()
            ))
        );
    }

    #[test]
    fn a_malformed_arkiv_section_is_refused() {
        let mut unknown = arkiv(|_| {});
        unknown["metering"] = json!({});
        for (arkiv, expected) in [
            (
                json!(null),
                "invalid type: null, expected an object with a `golemdb`",
            ),
            (json!({}), "missing field `golemdb`"),
            (unknown, "unknown field `metering`"),
            (
                arkiv(|g| g["hash_fn"] = g["hash_function"].take()),
                "unknown field `hash_fn`",
            ),
            (
                arkiv(|g| g["hash_function"] = json!("keccak256")),
                "unknown variant `keccak256`",
            ),
            (
                arkiv(|g| {
                    g.as_object_mut().unwrap().remove("cell_limits");
                }),
                "missing field `cell_limits`",
            ),
            (
                arkiv(|g| {
                    g.as_object_mut().unwrap().remove("record_limits");
                }),
                "missing field `record_limits`",
            ),
            (
                arkiv(|g| {
                    g.as_object_mut().unwrap().remove("metering");
                }),
                "missing field `metering`",
            ),
            (
                arkiv(|g| g["cell_limits"]["max_str_len"] = json!(-1)),
                "invalid value",
            ),
        ] {
            let error = with_arkiv(arkiv.clone()).unwrap_err().to_string();
            assert!(
                error.starts_with("genesis config.arkiv: ") && error.contains(expected),
                "{arkiv}: {error}"
            );
        }
    }
}
