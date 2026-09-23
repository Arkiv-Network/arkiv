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
use clap::{CommandFactory, FromArgMatches};
use node::ArkivNode;
use reth::{
    beacon_consensus::EthBeaconConsensus,
    cli::{Cli, Commands},
};
use reth_ethereum::chainspec::EthChainSpec;
use reth_node_ethereum::EthEvmConfig;
use std::sync::Arc;
use tracing::info;

fn main() {
    // Enable backtraces unless the caller already set a preference.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    let result = run();
    if let Err(err) = result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

fn run() -> eyre::Result<()> {
    let matches = Cli::<ArkivChainSpecParser>::command().get_matches();
    let cli = Cli::<ArkivChainSpecParser>::from_arg_matches(&matches)?;
    // Reth's offline component callback only receives a chainspec. Resolve the
    // same datadir from clap before moving the command into its runner.
    let mut command_matches = &matches;
    let mut datadir = None;
    loop {
        if command_matches.try_contains_id("datadir").unwrap_or(false) {
            datadir = Some(reth::args::DatadirArgs::from_arg_matches(command_matches)?);
        }
        match command_matches.subcommand() {
            Some((_, next)) => command_matches = next,
            None => break,
        }
    }
    let offline_store = if matches!(
        &cli.command,
        Commands::Import(_) | Commands::Stage(_) | Commands::ReExecute(_)
    ) {
        let spec = cli
            .command
            .chain_spec()
            .expect("execution commands have a chain");
        let dir = datadir
            .ok_or_else(|| eyre::eyre!("execution command has no datadir"))?
            .resolve_datadir(spec.chain());
        let store = arkiv_authenticated_store::Store::open(dir.data_dir().join("arkiv-state"))?;
        arkiv_reth_statemanager::genesis::initialize(spec.genesis(), store.clone())?;
        Some(store)
    } else {
        None
    };
    match cli {
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
            move |spec| {
                (
                    EthEvmConfig::new_with_evm_factory(spec.clone(), ArkivEvmFactory::new(offline_store.expect("offline execution store initialized"))),
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
                        let module = arkiv_reth_rpc::arkiv_module(ctx.provider().clone(), arkiv_authenticated_store::Store::open(ctx.config().datadir().data_dir().join("arkiv-state"))?)?;
                        ctx.modules.merge_configured(module)?;
                        info!(target: "arkiv-reth", "arkiv_* RPC namespace registered");
                        Ok(())
                    })
                    .launch_with_debug_capabilities()
                    .await?;
                handle.wait_for_node_exit().await
            },
        ),
    }
}
