//! The Arkiv black-box test harness.
//!
//! Two layers the tests build on:
//! - [`Node`] / [`NodeBuilder`] — spawn, kill, and restart the real `arkiv-reth`
//!   binary (kill-then-restart on the same datadir proves crash recovery).
//! - [`ArkivClient`] — a typed `arkiv_*` + `eth_*` RPC client to drive a node.
//!
//! Protocol constants a client needs ([`ARKIV_ADDRESS`], [`derive_entity_key`])
//! and the `--dev` test identities are re-exported here so a test imports from one
//! place. [`HarnessConfig`] remains the topology descriptor for the (future)
//! multi-node network runner.

use std::path::PathBuf;

mod client;
mod node;

pub use client::{ArkivClient, connect, connect_reader, hex_quantity, result_keys};
pub use node::{Node, NodeBuilder};

// Protocol re-exports: build calldata and predict minted keys from one import.
pub use arkiv_reth_executor::{ARKIV_ADDRESS, derive_entity_key};

/// Version of this support library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `reth --dev` chain id.
pub const DEV_CHAIN_ID: u64 = 1337;

/// Test-mnemonic account #0 — one of the accounts `reth --dev` pre-funds.
pub const DEV_KEY_0: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Test-mnemonic account #1 — also pre-funded by `--dev`; a handy non-owner.
pub const DEV_KEY_1: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// Endpoints the harness uses to reach a running execution + consensus pair.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Execution-layer JSON-RPC (`eth_*`) endpoint.
    pub el_rpc_url: String,
    /// Authenticated Engine API endpoint (CL <-> EL).
    pub el_engine_url: String,
    /// Consensus-layer beacon HTTP API endpoint.
    pub cl_beacon_url: String,
    /// Shared JWT secret used to authenticate the Engine API.
    pub jwt_path: PathBuf,
    /// Bespoke base-chain JSON-RPC endpoint — the DA / settlement substrate
    /// below the Arkiv chain (plain reth, no Arkiv semantics).
    pub base_chain_rpc_url: String,
    /// Inbox contract the committer posts DA to, on the base chain (contract C2).
    /// Zero until Contracts deploys it; T4 decodes its `BlockPosted` logs with
    /// `arkiv_da::decode_block`.
    pub inbox_address: String,
}

impl Default for HarnessConfig {
    /// Defaults match the deterministic host ports kurtosis publishes for our
    /// enclave: the Arkiv chain via ethereum-package `port_publisher`
    /// (EL block base 32000, CL base 33000) and the base chain via the
    /// `public_ports` pin in `kurtosis/base-chain/main.star`. No discovery —
    /// these are fixed by config, not allocated at random.
    fn default() -> Self {
        Self {
            // EL block (base 32000): rpc=+3, engine-rpc=+1.
            el_rpc_url: "http://127.0.0.1:32003".to_string(),
            el_engine_url: "http://127.0.0.1:32001".to_string(),
            // CL block (base 33000): beacon http=+1.
            cl_beacon_url: "http://127.0.0.1:33001".to_string(),
            jwt_path: PathBuf::from("jwt.hex"),
            base_chain_rpc_url: "http://127.0.0.1:18545".to_string(),
            inbox_address: "0x0000000000000000000000000000000000000000".to_string(),
        }
    }
}
