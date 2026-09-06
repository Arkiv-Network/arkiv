//! The three shapes a seeded state leaves in: a genesis JSON, an alloc-only
//! JSON for ethereum-package, or a `reth init-state` JSONL dump with its
//! `stateHash` genesis.

use std::collections::BTreeMap;
use std::io::Write;

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256};
use alloy_trie::root::state_root_ref_unhashed;
use eyre::{Result, bail};
use serde::Serialize;

use crate::build::SeededState;

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

/// Merge `base` (funding, predeploys) into the seeded accounts, refusing any
/// address both sides claim — a seed never overwrites an operator's account,
/// and an operator's account never silently replaces an entity's.
pub fn merge_alloc(
    seeded: &mut BTreeMap<Address, GenesisAccount>,
    base: BTreeMap<Address, GenesisAccount>,
) -> Result<()> {
    for (address, account) in base {
        if seeded.contains_key(&address) {
            bail!("account {address} is both in the base alloc and written by the seed");
        }
        seeded.insert(address, account);
    }
    Ok(())
}

/// Fold `base` into `state`: merge the accounts and restate the manifest's
/// root and account count over the whole alloc.
pub fn finish(state: &mut SeededState, base: BTreeMap<Address, GenesisAccount>) -> Result<()> {
    merge_alloc(&mut state.alloc, base)?;
    state.manifest.state_root = state_root_ref_unhashed(state.alloc.iter());
    state.manifest.accounts = state.alloc.len() as u64;
    Ok(())
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

/// One account per line, as `reth init-state` reads it: the genesis-account
/// fields plus `address`, after a first line naming the state root.
#[derive(Serialize)]
struct DumpLine<'a> {
    address: &'a Address,
    #[serde(flatten)]
    account: &'a GenesisAccount,
}

/// Write `alloc` as a `reth init-state` state dump. The first line carries
/// `root`, which the importer checks against the genesis header before it
/// reads a single account, and against the trie it rebuilds after the last.
pub fn write_state_dump<W: Write>(
    mut out: W,
    root: B256,
    alloc: &BTreeMap<Address, GenesisAccount>,
) -> Result<()> {
    serde_json::to_writer(&mut out, &serde_json::json!({ "root": root }))?;
    out.write_all(b"\n")?;
    for (address, account) in alloc {
        serde_json::to_writer(&mut out, &DumpLine { address, account })?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    fn funded(byte: u8) -> (Address, GenesisAccount) {
        (
            Address::repeat_byte(byte),
            GenesisAccount {
                balance: U256::from(7u64),
                ..Default::default()
            },
        )
    }

    #[test]
    fn dev_genesis_parses_with_its_chain_id() {
        let genesis = dev_genesis(4242);
        assert_eq!(genesis.config.chain_id, 4242);
        assert_eq!(genesis.base_fee_per_gas, Some(1_000_000_000));
        assert!(genesis.alloc.is_empty());
    }

    #[test]
    fn merge_refuses_overlap() {
        let mut seeded: BTreeMap<_, _> = [funded(1)].into_iter().collect();
        assert!(merge_alloc(&mut seeded, [funded(1)].into_iter().collect()).is_err());
        merge_alloc(&mut seeded, [funded(2)].into_iter().collect()).unwrap();
        assert_eq!(seeded.len(), 2);
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

    #[test]
    fn state_dump_lines_are_reth_shaped() {
        let (address, mut account) = funded(3);
        account.nonce = Some(1);
        account.code = Some(vec![0xFE, 0x01].into());
        account.storage = Some(
            [(B256::repeat_byte(1), B256::repeat_byte(2))]
                .into_iter()
                .collect(),
        );
        let alloc: BTreeMap<_, _> = [(address, account)].into_iter().collect();
        let mut out = Vec::new();
        write_state_dump(&mut out, B256::repeat_byte(4), &alloc).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let root: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            root["root"],
            serde_json::to_value(B256::repeat_byte(4)).unwrap()
        );
        let line: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(line["address"], serde_json::to_value(address).unwrap());
        assert_eq!(line["nonce"], "0x1");
        assert_eq!(line["balance"], "0x7");
        assert_eq!(line["code"], "0xfe01");
        assert!(line["storage"].is_object());
    }
}
