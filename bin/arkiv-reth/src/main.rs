//! `arkiv-reth`: a reth Ethereum node with the Arkiv entity engine wired in.
//!
//! This is the assembly point. We keep reth as the host (networking, txpool,
//! JSON-RPC, MDBX, engine/Engine API, sync) and add two things on top:
//!
//! 1. **The executor** — replaced by [`arkiv_reth_executor::ArkivExecutorBuilder`],
//!    the no-EVM entity state transition (a call to `ARKIV_ADDRESS` creates,
//!    updates, transfers, and expires entities).
//! 2. **The `arkiv_*` RPC** — the read surface the SDK depends on, built by
//!    [`arkiv_reth_rpc::arkiv_module`] and injected via `extend_rpc_modules`:
//!    `arkiv_getEntity`, `arkiv_query`, `arkiv_getEntityCount`,
//!    `arkiv_getBlockTiming`.
//!
//! The node runs on its own [`node::ArkivNode`] types instead of `EthereumNode`
//! because it is generic over [`arkiv_reth_chainspec::ArkivChainSpec`], where
//! reth's is hard-wired to reth's `ChainSpec`.
//!
//! The node still speaks the Ethereum interface a Lighthouse CL and the SDK expect.
//!
//! One reth subcommand is re-routed: `init-state`, which reth v2.5.0 cannot
//! run at genesis under its default storage layout. See [`init_state`].

mod init_state;
mod node;

// jemalloc, as reth's own binary does it. reth fragments badly under the stock
// system allocator on long syncs, and the allocator can only be chosen by the
// binary — enabling reth's "jemalloc" feature alone just turns on its stats
// reporting.
#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// Pulls jemalloc-sys into the link so the allocator override actually takes.
#[cfg(unix)]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

use arkiv_reth_chainspec::ArkivChainSpecParser;
use arkiv_reth_executor::ArkivEvmFactory;
use clap::Parser;
use node::ArkivNode;
use reth::{
    beacon_consensus::EthBeaconConsensus,
    cli::{Cli, Commands},
};
use reth_node_ethereum::EthEvmConfig;
use std::sync::Arc;
use tracing::info;

fn main() {
    // Enable backtraces unless the caller already set a preference.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // reth's log file records `debug` from every crate. turso, the SQLite
    // behind the chain pruning map, traces every page read and B-tree step at
    // that level: some forty lines per row written, which made the genesis
    // pruning bootstrap ten times slower than the database itself. The file
    // stays at debug for everything else; `--log.file.filter` still overrides.
    let _ = reth::args::DefaultLogArgs::default()
        .with_log_file_filter("debug,turso_core=info,turso=info".to_owned())
        .try_init();

    let result = match Cli::<ArkivChainSpecParser>::parse() {
        // The genesis-aware `init-state`; every other command is reth's.
        Cli {
            command: Commands::InitState(command),
            logs,
            ..
        } => init_state::run(command, logs),
        // `run_with_components` rather than `run`: the latter is bound to reth's
        // `ChainSpec`. The components closure gives the non-`node` subcommands
        // (`init`, `import`, `db`, ...) the same executor and consensus the node uses.
        cli => cli.run_with_components::<ArkivNode>(
            |spec| {
                (
                    EthEvmConfig::new_with_evm_factory(spec.clone(), ArkivEvmFactory::default()),
                    Arc::new(EthBeaconConsensus::new(spec)),
                )
            },
            async move |builder, _| {
                info!(target: "arkiv-reth", "Launching arkiv-reth (reth host + Arkiv entity engine)");
                let handle = builder
                    // Arkiv node types: Ethereum's, on ArkivChainSpec, with our executor.
                    .node(ArkivNode)
                    // Register the arkiv_* JSON-RPC namespace over reth's rpc modules.
                    .extend_rpc_modules(|ctx| {
                        let db = arkiv_store::ArkivDb::shared(
                            &arkiv_reth_executor::arkiv_db_path(ctx.config().datadir().data_dir()),
                            tokio::runtime::Handle::current(),
                        )?;
                        let module = arkiv_reth_rpc::arkiv_module(ctx.provider().clone(), db)?;
                        ctx.modules.merge_configured(module)?;
                        info!(target: "arkiv-reth", "arkiv_* RPC namespace registered");
                        Ok(())
                    })
                    .launch_with_debug_capabilities()
                    .await?;
                handle.wait_for_node_exit().await
            },
        ),
    };
    if let Err(err) = result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
