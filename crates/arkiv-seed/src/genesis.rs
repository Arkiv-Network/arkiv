//! The genesis JSON a seed is written for, and the one it leaves behind.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256};
use eyre::{Result, WrapErr};
use serde_json::Value;

/// The genesis field naming the block-0 state root: the chain spec parser's.
pub use arkiv_reth_chainspec::STATE_HASH_FIELD;

/// A geth-format genesis for a post-merge Arkiv devnet, every fork active from
/// block 0 and the base fee floored at 1 gwei -- the shape `--dev` runs on, as
/// a file. Chain id `chain_id`, nothing funded.
pub fn dev_genesis(chain_id: u64) -> Value {
    serde_json::json!({
        "config": {
            "chainId": chain_id,
            "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
            "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "mergeNetsplitBlock": 0, "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3b9aca00",
        "alloc": {}
    })
}

/// One funded account, as the genesis alloc spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Funded {
    pub balance: U256,
    pub nonce: u64,
}

/// A genesis's chain id.
pub fn chain_id(genesis: &Value) -> Result<u64> {
    genesis["config"]["chainId"]
        .as_u64()
        .ok_or_else(|| eyre::eyre!("the genesis has no config.chainId"))
}

/// A genesis's `stateHash`, if it names one.
pub fn state_hash(genesis: &Value) -> Result<Option<B256>> {
    genesis
        .get(STATE_HASH_FIELD)
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()).wrap_err(STATE_HASH_FIELD))
        .transpose()
}

/// A genesis's alloc, read with alloy's `GenesisAccount` -- the type the
/// node's chain spec parses it with, so every spelling the node accepts (a
/// nonce as a number, a decimal or a hex string) reads the same here. Code and
/// storage are dropped: a GolemDB genesis holds balances and nonces, and
/// nothing else of an account.
pub fn alloc(genesis: &Value) -> Result<BTreeMap<Address, Funded>> {
    let Some(entries) = genesis.get("alloc").filter(|value| !value.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let entries: BTreeMap<Address, alloy_genesis::GenesisAccount> =
        serde_json::from_value(entries.clone()).wrap_err("alloc")?;
    Ok(entries
        .into_iter()
        .map(|(address, account)| {
            (
                address,
                Funded {
                    balance: account.balance,
                    nonce: account.nonce.unwrap_or_default(),
                },
            )
        })
        .collect())
}

/// Fund `accounts` in `genesis`, leaving any account it already funds as it is.
pub fn fund(genesis: &mut Value, accounts: impl IntoIterator<Item = (Address, U256)>) {
    if !genesis["alloc"].is_object() {
        genesis["alloc"] = serde_json::json!({});
    }
    let alloc = genesis["alloc"]
        .as_object_mut()
        .expect("just made an object");
    let present: Vec<Address> = alloc.keys().filter_map(|key| key.parse().ok()).collect();
    for (address, balance) in accounts {
        if !present.contains(&address) {
            alloc.insert(
                address.to_string(),
                serde_json::json!({ "balance": format!("{balance:#x}") }),
            );
        }
    }
}

/// The hash of the genesis block `genesis` describes, exactly as the node
/// computes it from the same file.
pub fn genesis_hash(genesis: &Value) -> Result<B256> {
    use reth_chainspec::EthChainSpec as _;
    use reth_cli::chainspec::ChainSpecParser as _;
    let spec = arkiv_reth_chainspec::ArkivChainSpecParser::parse(&genesis.to_string())
        .wrap_err("parse the genesis as the node would")?;
    Ok(spec.genesis_hash())
}

/// How many alloc accounts carry code or storage -- which a GolemDB genesis
/// commit does not hold: it keeps balances and nonces.
pub fn accounts_with_code(genesis: &Value) -> usize {
    genesis["alloc"].as_object().map_or(0, |alloc| {
        alloc
            .values()
            .filter(|account| {
                let code = account["code"]
                    .as_str()
                    .is_some_and(|code| code != "0x" && !code.is_empty());
                let storage = account["storage"]
                    .as_object()
                    .is_some_and(|slots| !slots.is_empty());
                code || storage
            })
            .count()
    })
}

/// Empty the alloc: what a genesis naming its state root carries, its
/// accounts being inside that root.
pub fn clear_alloc(genesis: &mut Value) {
    genesis["alloc"] = serde_json::json!({});
}

/// Name `root` as the genesis state root.
pub fn set_state_hash(genesis: &mut Value, root: B256) {
    genesis[STATE_HASH_FIELD] = serde_json::to_value(root).expect("a B256 serializes");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_keeps_what_the_base_funds() {
        let mut genesis = dev_genesis(7);
        let one = Address::repeat_byte(1);
        fund(&mut genesis, [(one, U256::from(5))]);
        fund(
            &mut genesis,
            [
                (one, U256::from(9)),
                (Address::repeat_byte(2), U256::from(3)),
            ],
        );
        let alloc = alloc(&genesis).unwrap();
        assert_eq!(alloc[&one].balance, U256::from(5));
        assert_eq!(alloc[&Address::repeat_byte(2)].balance, U256::from(3));
        assert_eq!(chain_id(&genesis).unwrap(), 7);
    }

    #[test]
    fn the_state_hash_moves_the_genesis_hash() {
        let mut genesis = dev_genesis(1);
        let plain = genesis_hash(&genesis).unwrap();
        set_state_hash(&mut genesis, B256::repeat_byte(4));
        assert_ne!(genesis_hash(&genesis).unwrap(), plain);
    }

    #[test]
    fn an_emptied_alloc_keeps_the_genesis_hash() {
        // The header carries the stateHash either way, and nothing else of
        // the alloc reaches it.
        let mut genesis = dev_genesis(1);
        fund(&mut genesis, [(Address::repeat_byte(1), U256::from(5))]);
        set_state_hash(&mut genesis, B256::repeat_byte(4));
        let with_alloc = genesis_hash(&genesis).unwrap();
        clear_alloc(&mut genesis);
        assert_eq!(genesis_hash(&genesis).unwrap(), with_alloc);
        assert!(alloc(&genesis).unwrap().is_empty());
    }

    /// The consensus-genesis generator writes nonces as decimal strings.
    #[test]
    fn an_alloc_reads_in_every_spelling_the_node_accepts() {
        let mut genesis = dev_genesis(1);
        genesis["alloc"] = serde_json::json!({
            "0x0000000000000000000000000000000000000001": {"balance": "0x10", "nonce": "1"},
            "0x0000000000000000000000000000000000000002": {"balance": "16", "nonce": "0x2"},
            "0x0000000000000000000000000000000000000003": {"balance": "0x0", "nonce": 3},
        });
        let alloc = alloc(&genesis).unwrap();
        let nonces: Vec<u64> = alloc.values().map(|funded| funded.nonce).collect();
        assert_eq!(nonces, [1, 2, 3]);
        assert_eq!(alloc[&Address::with_last_byte(2)].balance, U256::from(16));
    }

    #[test]
    fn accounts_with_code_or_storage_are_counted() {
        let mut genesis = dev_genesis(1);
        genesis["alloc"] = serde_json::json!({
            "0x0000000000000000000000000000000000000001": {"balance": "0x1"},
            "0x0000000000000000000000000000000000000002": {"balance": "0x0", "code": "0x60"},
            "0x0000000000000000000000000000000000000003": {"balance": "0x0", "storage": {"0x00": "0x01"}},
        });
        assert_eq!(accounts_with_code(&genesis), 2);
    }

    #[test]
    fn the_state_hash_round_trips() {
        let mut genesis = dev_genesis(1);
        assert_eq!(state_hash(&genesis).unwrap(), None);
        set_state_hash(&mut genesis, B256::repeat_byte(4));
        assert_eq!(state_hash(&genesis).unwrap(), Some(B256::repeat_byte(4)));
    }
}
