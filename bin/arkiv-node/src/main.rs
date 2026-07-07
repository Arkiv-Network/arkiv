//! `arkiv-node`: a reth Ethereum node with the Arkiv entity engine wired in.
//!
//! This is the assembly point. We keep reth as the host (networking, txpool,
//! JSON-RPC, MDBX, engine/Engine API, sync) and add two things on top:
//!
//! 1. **The executor** — replaced by [`arkiv_reth_executor::ArkivExecutorBuilder`],
//!    the no-EVM entity state transition (a call to `ARKIV_ADDRESS` creates,
//!    updates, transfers, and expires entities).
//! 2. **The `arkiv_*` RPC** — the read surface the SDK depends on, injected via
//!    `extend_rpc_modules` (see [`rpc`]). Today: `arkiv_getEntity`.
//!
//! The node still speaks the Ethereum interface a Lighthouse CL and the SDK expect.

mod rpc;
mod snapshot;

use arkiv_reth_executor::ArkivExecutorBuilder;
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
        info!(target: "arkiv-node", "Launching arkiv-node (reth host + Arkiv entity engine)");
        let handle = builder
            // Standard Ethereum node types (primitives, chainspec, payload, storage).
            .with_types::<EthereumNode>()
            // Default Ethereum components, but our executor replaces the EVM one.
            .with_components(EthereumNode::components().executor(ArkivExecutorBuilder::default()))
            // Standard Ethereum add-ons (RPC, engine API, validator).
            .with_add_ons(EthereumAddOns::default())
            // Register the arkiv_* JSON-RPC namespace over reth's rpc modules.
            .extend_rpc_modules(|ctx| {
                let module = rpc::arkiv_module(ctx.provider().clone())?;
                ctx.modules.merge_configured(module)?;
                info!(target: "arkiv-node", "arkiv_* RPC namespace registered");
                Ok(())
            })
            .launch_with_debug_capabilities()
            .await?;
        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
