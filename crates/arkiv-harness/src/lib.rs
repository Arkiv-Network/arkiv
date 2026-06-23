//! Shared configuration and topology types for the Arkiv black-box harness.
//!
//! Deliberately dependency-free for now: it just describes how to reach a
//! running EL + CL pair. Container orchestration and RPC clients build on top
//! of this in a later leg.

use std::path::PathBuf;

/// Version of this support library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
