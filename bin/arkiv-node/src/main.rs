//! `arkiv-node`: a reth Ethereum node with the Arkiv executor wired in.
//!
//! This is the assembly point. We keep reth as the host (networking, txpool,
//! JSON-RPC, MDBX, engine/Engine API, sync) and override exactly one component:
//! the **executor**, replaced by [`arkiv_executor::ArkivExecutorBuilder`]. The
//! node still speaks the Ethereum interface a Lighthouse CL and the SDK expect.
//!
//! Today the Arkiv executor is the stock Ethereum executor with an Arkiv
//! precompile registered at `ARKIV_ADDRESS` — it proves the injection wiring end
//! to end. The neutered-EVM policy and the entity engine land behind that same
//! seam without touching this file (see experiments/post-evm-execution-report.md).

use arkiv_executor::ArkivExecutorBuilder;
use clap::Parser;
use reth::cli::Cli;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_ethereum::{EthereumNode, node::EthereumAddOns};
use tracing::info;

fn main() {
    // Enable backtraces unless the caller already set a preference.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "arkiv-node", "Launching arkiv-node (reth host + Arkiv executor)");
        let handle = builder
            // Standard Ethereum node types (primitives, chainspec, payload, storage).
            .with_types::<EthereumNode>()
            // Default Ethereum components, but our executor replaces the EVM one.
            .with_components(EthereumNode::components().executor(ArkivExecutorBuilder::default()))
            // Standard Ethereum add-ons (RPC, engine API, validator).
            .with_add_ons(EthereumAddOns::default())
            .launch_with_debug_capabilities()
            .await?;
        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
