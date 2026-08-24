//! Genesis primitives for the Arkiv chain.
//!
//! Provides:
//! - [`ARKIV_ADDRESS`] (`0x44…0044`) — the precompile's registration
//!   address. No genesis presence; the custom `EvmFactory` registers
//!   the precompile here programmatically.
//! - A convenience helper that builds the dev-funding `alloc`.
//!
//! Used by:
//!   - `arkiv-cli inject-predeploy`: splices dev-funded accounts into a
//!     geth-format genesis JSON. The command name is legacy; no bytecode
//!     is deployed at [`ARKIV_ADDRESS`].

// Re-export so consumers (e.g. `arkiv-cli inject-predeploy`) don't need to
// take a direct dep on alloy-genesis.
pub use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, U256};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use eyre::{Result, bail};
use std::collections::BTreeMap;

/// Address entity calldata is sent to, as an alloy [`Address`].
///
/// EOAs / SDKs `CALL` this address with the `execute(Operation[])` /
/// `nonces(address)` ABI declared by `IEntityRegistry`. No genesis
/// allocation is required — registration is programmatic, which is what
/// `tests::genesis_alloc_has_dev_funding_only` pins.
///
/// The bytes are [`arkiv_interfaces::constants::ethereum::ARKIV_RETH_ADDRESS`], shared with the
/// executor so genesis and execution cannot disagree about where the engine lives.
pub const ARKIV_ADDRESS: Address =
    Address::new(arkiv_interfaces::constants::ethereum::ARKIV_RETH_ADDRESS);

/// First account derived from [`ARKIV_DEV_MNEMONIC`] at standard BIP-44
/// path `m/44'/60'/0'/0/0`. Kept as a `const` so callers that only need
/// the well-known dev address don't have to derive at runtime.
///
/// Verified by [`tests::dev_address_matches_first_signer`].
pub const DEV_ADDRESS: Address = Address::new([
    0xf3, 0x9F, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xF6, 0xF4, 0xce, 0x6a, 0xB8, 0x82, 0x72, 0x79, 0xcf,
    0xff, 0xb9, 0x22, 0x66,
]);

/// Hardhat-compatible test mnemonic. The first 20 derived addresses match
/// the standard hardhat / foundry / anvil defaults; subsequent indices
/// (20..[`ARKIV_DEV_ACCOUNT_COUNT`]) are deterministic but novel.
///
/// **Do not use in production.** This phrase is published in every
/// JavaScript and Rust EVM testing toolkit.
pub const ARKIV_DEV_MNEMONIC: &str = "test test test test test test test test test test test junk";

/// Number of accounts derived from [`ARKIV_DEV_MNEMONIC`] and pre-funded
/// in the dev chainspec. Caps the simulator's signer pool size.
pub const ARKIV_DEV_ACCOUNT_COUNT: usize = 100;

/// Default per-account balance for [`dev_funding_alloc`]: 10,000 ETH.
pub fn arkiv_dev_balance_wei() -> U256 {
    U256::from(10_000u64) * U256::from(1_000_000_000_000_000_000u128)
}

/// Build the complete Arkiv dev funding alloc:
/// [`ARKIV_DEV_ACCOUNT_COUNT`] mnemonic-derived accounts each prefunded
/// with [`arkiv_dev_balance_wei`]. The Arkiv precompile and entitydb
/// system account are not genesis accounts.
pub fn genesis_alloc() -> Result<BTreeMap<Address, GenesisAccount>> {
    let mut alloc = BTreeMap::new();
    for (addr, acc) in dev_funding_alloc(ARKIV_DEV_ACCOUNT_COUNT, arkiv_dev_balance_wei())? {
        alloc.insert(addr, acc);
    }
    Ok(alloc)
}

/// Derive `count` `PrivateKeySigner`s from [`ARKIV_DEV_MNEMONIC`] at
/// standard BIP-44 paths `m/44'/60'/0'/0/{0..count}`.
///
/// The first 20 addresses match the well-known hardhat/foundry/anvil
/// defaults. Indices 20..100 are the same on every machine but aren't
/// part of any other tool's defaults.
pub fn dev_signers(count: usize) -> Result<Vec<PrivateKeySigner>> {
    if count > ARKIV_DEV_ACCOUNT_COUNT {
        bail!(
            "requested {} signers but only {} are funded in the dev chainspec",
            count,
            ARKIV_DEV_ACCOUNT_COUNT
        );
    }
    (0..count as u32)
        .map(|i| {
            MnemonicBuilder::<English>::default()
                .phrase(ARKIV_DEV_MNEMONIC)
                .index(i)
                .map_err(|e| eyre::eyre!("index {} invalid: {}", i, e))?
                .build()
                .map_err(|e| eyre::eyre!("signer {} build failed: {}", i, e))
        })
        .collect()
}

/// Derive `count` mnemonic addresses and pair each with a `GenesisAccount`
/// of `balance_wei` and no code/storage. Used by `arkiv-cli inject-funding`
/// and by [`genesis_alloc`].
pub fn dev_funding_alloc(
    count: usize,
    balance_wei: U256,
) -> Result<Vec<(Address, GenesisAccount)>> {
    Ok(dev_signers(count)?
        .into_iter()
        .map(|s| {
            (
                s.address(),
                GenesisAccount {
                    balance: balance_wei,
                    ..Default::default()
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_address_matches_first_signer() {
        let signers = dev_signers(1).expect("derive");
        assert_eq!(signers[0].address(), DEV_ADDRESS);
    }

    #[test]
    fn genesis_alloc_has_dev_funding_only() {
        let alloc = genesis_alloc().expect("alloc");
        assert!(
            !alloc.contains_key(&ARKIV_ADDRESS),
            "ARKIV_ADDRESS is a programmatic precompile target; no genesis presence",
        );
        assert_eq!(alloc.len(), ARKIV_DEV_ACCOUNT_COUNT);
    }
}
