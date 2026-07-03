//! `arkiv-node`: a reth Ethereum node with the Arkiv executor and `arkiv_*`
//! RPC namespace wired in.
//!
//! Assembly point: reth hosts networking, txpool, engine/Engine API, MDBX,
//! and the standard Ethereum RPC. We override two things:
//!
//! 1. **Executor** — replaced by [`arkiv_executor::ArkivExecutorBuilder`].
//!    All transactions are handled by a fixed-function state machine; the
//!    EVM is never entered.
//!
//! 2. **RPC** — the `arkiv_*` namespace is injected via `.extend_rpc_modules`,
//!    giving SDK clients access to `arkiv_query`, `arkiv_getEntityCount`, and
//!    `arkiv_getBlockTiming`.

mod rpc;
mod state_adapter;

use arkiv_executor::ArkivExecutorBuilder;
use clap::Parser;
use reth::cli::Cli;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use tracing::info;

use rpc::{ArkivApiServer, ArkivRpc};

fn main() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "arkiv-node", "Launching arkiv-node (reth host + Arkiv executor + arkiv_* RPC)");
        let handle = builder
            .with_types::<EthereumNode>()
            .with_components(
                EthereumNode::components().executor(ArkivExecutorBuilder::default()),
            )
            .with_add_ons(EthereumAddOns::default())
            .extend_rpc_modules(|ctx| {
                let provider = ctx.provider().clone();
                let ext = ArkivRpc::new(provider);
                ctx.modules.merge_configured(ext.into_rpc())?;
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
