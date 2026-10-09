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
use arkiv_reth_golemdbprovider::{ArkivProvider, ArkivProviderBuilder};
use arkiv_reth_statemanager::{BlockSeals, HostStore};
use clap::Parser;
use node::ArkivNode;
use reth::builder::{DebugNodeLauncher, Node, NodeTypesWithDBAdapter};
use reth_db::DatabaseEnv;

/// The node types the provider is generic over, with the database bound in.
type ArkivNodeTypes = NodeTypesWithDBAdapter<ArkivNode, DatabaseEnv>;
use reth::{beacon_consensus::EthBeaconConsensus, cli::Cli};
use reth_node_ethereum::EthEvmConfig;
use std::sync::Arc;
use tracing::info;

/// The store Arkiv's state lives in.
///
/// `ARKIV_STORE_URL` names a Valkey server; unset, the node runs the in-process
/// reference store, which is what `--dev` wants and what the black-box tests
/// spawn. Either way there is exactly one store, and it holds every entity,
/// the query index and the minting nonces.
fn open_store() -> eyre::Result<HostStore> {
    match std::env::var("ARKIV_STORE_URL") {
        Ok(url) => {
            let namespace =
                std::env::var("ARKIV_STORE_NAMESPACE").unwrap_or_else(|_| "arkiv".to_owned());
            let store = arkiv_valkey::ValkeyStore::connect(&url, &namespace)
                .map_err(|e| eyre::eyre!("connect to the Arkiv store at {url}: {e:?}"))?;
            info!(target: "arkiv-reth", %url, %namespace, "Arkiv state on Valkey");
            Ok(Arc::new(store))
        }
        Err(_) => {
            info!(target: "arkiv-reth", "Arkiv state in-process (set ARKIV_STORE_URL for Valkey)");
            Ok(Arc::new(arkiv_interfaces::store::reference::MemStore::new()))
        }
    }
}

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

    // `run_with_components` rather than `run`: the latter is bound to reth's
    // `ChainSpec`. The components closure gives the non-`node` subcommands
    // (`init`, `import`, `db`, ...) the same executor and consensus the node uses.
    let store = match open_store() {
        Ok(store) => store,
        Err(err) => {
            eprintln!("Error: {err:?}");
            std::process::exit(1);
        }
    };
    let seals = Arc::new(BlockSeals::new());
    let factory_store = store.clone();
    let factory_seals = seals.clone();
    let result = Cli::<ArkivChainSpecParser>::parse().run_with_components::<ArkivNode>(
        move |spec| {
            (
                EthEvmConfig::new_with_evm_factory(
                    spec.clone(),
                    ArkivEvmFactory::new(factory_store.clone(), factory_seals.clone()),
                ),
                Arc::new(EthBeaconConsensus::new(spec)),
            )
        },
        async move |builder, _| {
            info!(target: "arkiv-reth", "Launching arkiv-reth (reth host + Arkiv entity engine)");
            let rpc_store = store.clone();
            // Arkiv node types: Ethereum's, on ArkivChainSpec, with our executor.
            let node = ArkivNode::new(store.clone(), seals.clone());
            // Spelt out rather than `.node(node)`: that convenience pins the
            // provider to reth's `BlockchainProvider`, and the whole point here
            // is to run on ours. `with_types_and_provider` names the provider
            // and `with_provider_builder` says how to build it; the two have to
            // agree or the launcher will not typecheck.
            let builder = builder
                .with_types_and_provider::<ArkivNode, ArkivProvider<ArkivNodeTypes>>()
                .with_components(node.components_builder())
                .with_add_ons(node.add_ons())
                // Register the arkiv_* JSON-RPC namespace over reth's rpc modules.
                .extend_rpc_modules(move |ctx| {
                    let module =
                        arkiv_reth_rpc::arkiv_module(ctx.provider().clone(), rpc_store.clone())?;
                    ctx.modules.merge_configured(module)?;
                    info!(target: "arkiv-reth", "arkiv_* RPC namespace registered");
                    Ok(())
                });
            let launcher = builder
                .engine_api_launcher()
                .with_provider_builder(ArkivProviderBuilder::new(store.clone()));
            let handle = builder
                .launch_with(DebugNodeLauncher::new(launcher))
                .await?;
            handle.wait_for_node_exit().await
        },
    );
    if let Err(err) = result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
