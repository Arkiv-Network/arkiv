//! Black-box test harness driver (skeleton).
//!
//! For now this only resolves the target topology and prints it, so the wiring
//! is verifiable end-to-end before any real orchestration exists. Container
//! lifecycle management and the EL/CL keep-up assertions land in a later leg.

use arkiv_harness::{HarnessConfig, VERSION};

fn main() {
    let cfg = HarnessConfig::default();

    println!("arkiv-test-harness v{VERSION}");
    println!("planned topology:");
    println!("  EL JSON-RPC : {}", cfg.el_rpc_url);
    println!("  EL Engine   : {}", cfg.el_engine_url);
    println!("  CL beacon   : {}", cfg.cl_beacon_url);
    println!("  JWT secret  : {}", cfg.jwt_path.display());
    println!("  Base chain  : {}", cfg.base_chain_rpc_url);
}
