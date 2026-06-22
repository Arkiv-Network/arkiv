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
}

impl Default for HarnessConfig {
    /// Defaults match the `docker-compose.yml` service topology, reached from
    /// the host via published ports.
    fn default() -> Self {
        Self {
            el_rpc_url: "http://127.0.0.1:8545".to_string(),
            el_engine_url: "http://127.0.0.1:8551".to_string(),
            cl_beacon_url: "http://127.0.0.1:5052".to_string(),
            jwt_path: PathBuf::from("jwt.hex"),
        }
    }
}
