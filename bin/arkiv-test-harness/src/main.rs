//! Black-box test harness driver (skeleton).
//!
//! Resolves the target topology and prints it, so the wiring is verifiable
//! before any real orchestration exists.

use arkiv_harness::{HarnessConfig, VERSION};

fn main() {
    let cfg = HarnessConfig::default();

    println!("arkiv-test-harness v{VERSION}");
    println!("planned topology:");
    println!("  EL JSON-RPC : {}", cfg.el_rpc_url);
    println!("  EL Engine   : {}", cfg.el_engine_url);
    println!("  CL beacon   : {}", cfg.cl_beacon_url);
    println!("  JWT secret  : {}", cfg.jwt_path.display());
}
