//! Dummy `arkiv-node`: a vanilla reth Ethereum node.
//!
//! This stands in for the real arkiv-op-reth execution client so the harness has
//! an EL to drive. Arkiv precompile / entity semantics are intentionally absent —
//! this is just `EthereumNode` behind the standard reth CLI. It speaks the same
//! Engine API a Lighthouse CL expects, which is all the harness needs for now.

use clap::Parser;
use reth::cli::Cli;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_ethereum::EthereumNode;
use tracing::info;

fn main() {
    // Enable backtraces unless the caller already set a preference.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "arkiv-node", "Launching dummy arkiv-node (vanilla reth)");
        let handle = builder
            .node(EthereumNode::default())
            .launch_with_debug_capabilities()
            .await?;
        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
