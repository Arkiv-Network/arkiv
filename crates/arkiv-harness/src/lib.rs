//! The Arkiv black-box test harness.
//!
//! Two layers the tests build on:
//! - [`Node`] / [`NodeBuilder`] — spawn, kill, and restart the real `arkiv-reth`
//!   binary (kill-then-restart on the same datadir proves crash recovery).
//! - [`ArkivClient`] — a typed `arkiv_*` + `eth_*` RPC client to drive a node.
//!
//! Protocol constants a client needs ([`ARKIV_ADDRESS`], [`derive_entity_address`])
//! and the `--dev` test identities are re-exported here so a test imports from one
//! place.

mod client;
mod node;

pub use client::{ArkivClient, connect, connect_reader, hex_quantity, result_keys};
pub use node::{Node, NodeBuilder};

// Protocol re-exports: build calldata and predict minted keys from one import.
pub use arkiv_interfaces::primitives::EntityCreationNonce;
pub use arkiv_reth_executor::{ARKIV_ADDRESS, derive_entity_address};

/// Version of this support library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `reth --dev` chain id.
pub const DEV_CHAIN_ID: u64 = 1337;

/// Test-mnemonic account #0 — one of the accounts `reth --dev` pre-funds.
pub const DEV_KEY_0: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Test-mnemonic account #1 — also pre-funded by `--dev`; a handy non-owner.
pub const DEV_KEY_1: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
