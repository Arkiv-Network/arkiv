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
use std::sync::{Arc, OnceLock};
use tracing::info;

/// The store Arkiv's state lives in.
///
/// `ARKIV_STORE_URL` chooses it: `golemdb://<directory>` opens GolemDB in-process
/// in that directory, any other URL names a Valkey server, and unset runs the
/// in-process reference store, which is what `--dev` wants and what the
/// black-box tests spawn. Either way there is exactly one store, and it holds
/// every entity, the query index and the minting nonces.
///
/// A relative GolemDB directory is taken from the working directory, not from
/// `--datadir`.
fn open_store() -> eyre::Result<HostStore> {
    let Ok(url) = std::env::var("ARKIV_STORE_URL") else {
        info!(target: "arkiv-reth", "Arkiv state in-process (set ARKIV_STORE_URL for GolemDB or Valkey)");
        return Ok(Arc::new(arkiv_interfaces::store::reference::MemStore::new()));
    };
    if let Some(directory) = url.strip_prefix("golemdb://") {
        let store = arkiv_golemdb::GolemDbStore::open(directory.as_ref(), &golemdb_config())
            .map_err(|e| eyre::eyre!("open GolemDB at {directory}: {e}"))?;
        info!(target: "arkiv-reth", %directory, "Arkiv state on GolemDB");
        return Ok(Arc::new(store));
    }
    let namespace = std::env::var("ARKIV_STORE_NAMESPACE").unwrap_or_else(|_| "arkiv".to_owned());
    let store = arkiv_valkey::ValkeyStore::connect(&url, &namespace)
        .map_err(|e| eyre::eyre!("connect to the Arkiv store at {url}: {e:?}"))?;
    info!(target: "arkiv-reth", %url, %namespace, "Arkiv state on Valkey");
    Ok(Arc::new(store))
}

/// GolemDB's settings for Arkiv, from Arkiv's own limits and segments. They are
/// fixed when a database is created and part of its state root, so every node
/// of a chain must use the same.
fn golemdb_config() -> arkiv_golemdb::GolemDbConfig {
    use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_PAYLOAD_BYTES, MAX_STR_BYTES};
    let limit = |bytes: usize| u32::try_from(bytes).expect("Arkiv's limits fit in u32");
    arkiv_golemdb::GolemDbConfig {
        hash_function: arkiv_golemdb::HashAlgorithm::Keccak256,
        cell_limits: arkiv_golemdb::CellLimits {
            max_cell_name_len: limit(MAX_ATTRIBUTE_NAME_BYTES),
            max_str_len: limit(MAX_STR_BYTES),
            max_bytes_len: limit(MAX_PAYLOAD_BYTES),
        },
        segments: arkiv_reth_statemanager::segments::ALL
            .iter()
            .map(|&(name, columns)| arkiv_golemdb::SegmentSpec {
                name: name.to_owned(),
                columns,
            })
            .collect(),
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

    // The store opens on first use: after the command line is parsed and
    // tracing is up, and only for a command that runs Arkiv's state. So
    // `--version` creates no database, and `db` does not trip over the lock of
    // a node running on the same one.
    let store = Arc::new(OnceLock::<HostStore>::new());
    let open = move || {
        store
            .get_or_init(|| open_store().unwrap_or_else(|err| exit_with(&err)))
            .clone()
    };
    let seals = Arc::new(BlockSeals::new());
    let factory_seals = seals.clone();
    let factory_open = open.clone();
    // `run_with_components` rather than `run`: the latter is bound to reth's
    // `ChainSpec`. The components closure gives the non-`node` subcommands
    // (`import`, `stage`, `re-execute`) the same executor and consensus the node
    // uses.
    let result = Cli::<ArkivChainSpecParser>::parse().run_with_components::<ArkivNode>(
        move |spec| {
            (
                EthEvmConfig::new_with_evm_factory(
                    spec.clone(),
                    ArkivEvmFactory::new(factory_open(), factory_seals.clone()),
                ),
                Arc::new(EthBeaconConsensus::new(spec)),
            )
        },
        async move |builder, _| {
            info!(target: "arkiv-reth", "Launching arkiv-reth (reth host + Arkiv entity engine)");
            let store = open();
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
                .with_provider_builder(ArkivProviderBuilder::new(store.clone(), seals.clone()));
            let handle = builder
                .launch_with(DebugNodeLauncher::new(launcher))
                .await?;
            handle.wait_for_node_exit().await
        },
    );
    if let Err(err) = result {
        exit_with(&err);
    }
}

fn exit_with(err: &eyre::Report) -> ! {
    eprintln!("Error: {err:?}");
    std::process::exit(1);
}
