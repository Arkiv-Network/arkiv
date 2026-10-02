//! The genesis shapes a seeded state leaves in: a genesis JSON with the alloc
//! in it, or the `stateHash` genesis that goes with a `reth init-state` dump
//! (the dump itself is written by [`StreamSink`](crate::StreamSink)).

use std::collections::BTreeMap;

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256};
use eyre::Result;

/// The genesis field naming an explicit state root — geth's spelling, which
/// its `core.Genesis` honours when `alloc` is empty. The Arkiv chain spec
/// parser reads it the same way.
pub const STATE_HASH_FIELD: &str = "stateHash";

/// A geth-format genesis for a post-merge Arkiv devnet, every fork active from
/// block 0 and the base fee floored at 1 gwei — the shape `--dev` runs on, as a
/// file, so a seeded alloc can be merged in. Chain id `chain_id`.
pub fn dev_genesis(chain_id: u64) -> Genesis {
    serde_json::from_value(serde_json::json!({
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
    }))
    .expect("a well-formed genesis")
}

/// `genesis` with `alloc` replaced by `alloc` — the `--chain <file>` route.
pub fn genesis_with_alloc(
    mut genesis: Genesis,
    alloc: BTreeMap<Address, GenesisAccount>,
) -> Genesis {
    genesis.alloc = alloc;
    genesis
}

/// `genesis` with an empty `alloc` and `stateHash` set to `root` — the
/// `init-state` route. Serialized by hand because the field is not part of the
/// `Genesis` type.
pub fn genesis_with_state_hash(genesis: Genesis, root: B256) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(genesis_with_alloc(genesis, BTreeMap::new()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("genesis serialized to a non-object"))?;
    object.insert(STATE_HASH_FIELD.to_string(), serde_json::to_value(root)?);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dev_genesis_parses_with_its_chain_id() {
        let genesis = dev_genesis(4242);
        assert_eq!(genesis.config.chain_id, 4242);
        assert_eq!(genesis.base_fee_per_gas, Some(1_000_000_000));
        assert!(genesis.alloc.is_empty());
    }

    #[test]
    fn state_hash_genesis_has_the_field_and_no_alloc() {
        let value = genesis_with_state_hash(dev_genesis(1), B256::repeat_byte(9)).unwrap();
        assert_eq!(value["alloc"], serde_json::json!({}));
        assert_eq!(
            value[STATE_HASH_FIELD],
            serde_json::to_value(B256::repeat_byte(9)).unwrap()
        );
        // Still a genesis the alloy type reads (the extra field is ignored).
        let parsed: Genesis = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.config.chain_id, 1);
    }
}
