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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::info;

/// An example for the error messages.
const PATH_EXAMPLE: &str = "ARKIV_STORE_PATH=/var/lib/arkiv/golemdb";

/// Open the store Arkiv's state lives in. There is exactly one, and it holds
/// every entity, the query index and the minting nonces.
///
/// Exactly one of two variables names it:
/// - `ARKIV_STORE_PATH` (`path`) is GolemDB's directory. It must be absolute
///   or start with `~`, the home directory; otherwise it is taken as written.
/// - `ARKIV_EXPERIMENTAL_STORE` (`experimental`) is for tests and experiments:
///   `memory` is the in-process reference store, gone when the process is, and
///   a Valkey URL, such as `redis://127.0.0.1:6379`, is a Valkey server.
fn open_store(path: Option<OsString>, experimental: Option<OsString>) -> eyre::Result<HostStore> {
    match (path, experimental) {
        (Some(path), None) => {
            let path = expand_home(path.into())?;
            eyre::ensure!(
                path.is_absolute(),
                "ARKIV_STORE_PATH={}: the GolemDB directory must be an absolute path, such as \
                 {PATH_EXAMPLE} or ~/arkiv/golemdb",
                path.display()
            );
            open_golemdb(&path)
        }
        (None, Some(store)) => match store.to_str() {
            Some("memory") => {
                info!(target: "arkiv-reth", "Arkiv state in-process, lost when the node stops");
                Ok(Arc::new(arkiv_interfaces::store::reference::MemStore::new()))
            }
            // Every scheme Valkey's client takes: plain, TLS, cluster, sentinel, socket.
            Some(url)
                if url.split_once("://").is_some_and(|(scheme, _)| {
                    scheme.starts_with("redis") || scheme.starts_with("valkey")
                }) =>
            {
                open_valkey(url)
            }
            _ => eyre::bail!(
                "ARKIV_EXPERIMENTAL_STORE={}: use memory, or a Valkey URL such as \
                 redis://127.0.0.1:6379",
                store.to_string_lossy()
            ),
        },
        (None, None) => eyre::bail!("no store is named; set {PATH_EXAMPLE}, the GolemDB directory"),
        (Some(_), Some(_)) => eyre::bail!(
            "ARKIV_STORE_PATH and ARKIV_EXPERIMENTAL_STORE are both set; a node has one store"
        ),
    }
}

/// `path` with a leading `~` (`~` or `~/…`) replaced by the home directory, as
/// a shell would; a shell leaves it alone inside a variable's value.
fn expand_home(path: PathBuf) -> eyre::Result<PathBuf> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(path);
    };
    let home = std::env::home_dir().ok_or_else(|| {
        eyre::eyre!("ARKIV_STORE_PATH starts with ~, but there is no home directory")
    })?;
    Ok(home.join(rest))
}

/// Open GolemDB in `directory`, creating it with Arkiv's settings if it is new.
fn open_golemdb(directory: &Path) -> eyre::Result<HostStore> {
    let at = directory.display();
    let store =
        arkiv_golemdb::GolemDbStore::open(directory, &golemdb_config()).map_err(|e| match e {
            arkiv_golemdb::OpenError::GenesisMismatch => eyre::eyre!(
                "open GolemDB at {at}: it was created with other GolemDB settings, such as by \
                 another Arkiv version; use a new directory"
            ),
            e => eyre::eyre!("open GolemDB at {at}: {e}"),
        })?;
    info!(target: "arkiv-reth", directory = %at, "Arkiv state on GolemDB");
    Ok(Arc::new(store))
}

/// Connect to the Valkey server at `url`, in the namespace
/// `ARKIV_STORE_NAMESPACE` names (`arkiv` if unset).
fn open_valkey(url: &str) -> eyre::Result<HostStore> {
    let namespace = std::env::var("ARKIV_STORE_NAMESPACE").unwrap_or_else(|_| "arkiv".to_owned());
    let store = arkiv_valkey::ValkeyStore::connect(url, &namespace)
        .map_err(|e| eyre::eyre!("connect to the Arkiv store at {url}: {e:?}"))?;
    info!(target: "arkiv-reth", %url, %namespace, "Arkiv state on Valkey");
    Ok(Arc::new(store))
}

/// GolemDB's settings for Arkiv, from Arkiv's own limits and segments. They are
/// fixed when a database is created and part of its state root, so every node
/// of a chain must use the same.
fn golemdb_config() -> arkiv_golemdb::GolemDbConfig {
    use arkiv_interfaces::constants::{
        MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES,
    };
    let limit = |bytes: usize| u32::try_from(bytes).expect("Arkiv's limits fit in u32");
    arkiv_golemdb::GolemDbConfig {
        hash_function: arkiv_golemdb::HashAlgorithm::Keccak256,
        cell_limits: arkiv_golemdb::CellLimits {
            max_cell_name_len: limit(MAX_ATTRIBUTE_NAME_BYTES),
            max_str_len: limit(MAX_STR_BYTES),
            max_bytes_len: limit(MAX_BYTES_VALUE_LEN),
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
    //
    // It opens in `block_in_place`, outside async code: a store may own a
    // runtime of its own, as Valkey's does, and tokio refuses to drop one from
    // async code, which a failed connect does.
    let store = Arc::new(OnceLock::<HostStore>::new());
    let open = move || {
        store
            .get_or_init(|| {
                tokio::task::block_in_place(|| {
                    open_store(
                        std::env::var_os("ARKIV_STORE_PATH"),
                        std::env::var_os("ARKIV_EXPERIMENTAL_STORE"),
                    )
                })
                .unwrap_or_else(|err| {
                    // A setup mistake, so the message alone: no backtrace.
                    eprintln!("Error: {err:#}");
                    std::process::exit(1);
                })
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn set(value: impl Into<OsString>) -> Option<OsString> {
        Some(value.into())
    }

    /// Why `open_store` refused, for a call that must not open anything.
    fn refusal(path: Option<OsString>, experimental: Option<OsString>) -> String {
        match open_store(path, experimental) {
            Ok(_) => panic!("a store opened"),
            Err(error) => error.to_string(),
        }
    }

    /// The path is a directory name as written: no URL rules apply to it.
    #[test]
    fn golemdb_opens_in_the_directory_as_written() {
        let dir = tempfile::TempDir::new().unwrap();
        for name in ["my dir", "run#1", "a%2Fb?x"] {
            let path = dir.path().join(name);
            open_store(set(&path), None).expect(name);
            assert!(path.is_dir(), "{name}");
        }
    }

    #[test]
    fn golemdb_needs_an_absolute_path() {
        for relative in ["", "golemdb", "~user/x"] {
            let error = refusal(set(relative), None);
            assert!(error.contains("absolute path"), "{relative}: {error}");
        }
    }

    /// A leading `~` is the home directory, as in a shell; nothing else is.
    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let home = std::env::home_dir().unwrap();
        let expand = |path: &str| expand_home(path.into()).unwrap();
        assert_eq!(expand("~/arkiv/golemdb"), home.join("arkiv/golemdb"));
        assert_eq!(expand("~"), home);
        for as_written in ["/srv/golemdb", "/srv/~/x", "~user/x", "golemdb"] {
            assert_eq!(expand(as_written), PathBuf::from(as_written));
        }
    }

    #[test]
    fn exactly_one_store_is_named() {
        assert!(refusal(None, None).contains("no store is named"));
        let error = refusal(set("/srv/golemdb"), set("memory"));
        assert!(error.contains("both set"), "{error}");
    }

    #[test]
    fn an_experimental_store_is_memory_or_valkey() {
        assert!(open_store(None, set("memory")).is_ok());
        // Nothing listens on port 1, so this shows each was taken as Valkey.
        for valkey in [
            "redis://127.0.0.1:1",
            "rediss://127.0.0.1:1",
            "valkey://127.0.0.1:1",
        ] {
            let error = refusal(None, set(valkey));
            assert!(
                error.contains("connect to the Arkiv store"),
                "{valkey}: {error}"
            );
        }
    }

    /// A typo is not taken for some other store.
    #[test]
    fn an_unknown_experimental_store_is_refused() {
        for unknown in ["", "Memory", "valkey", "redis", "golemdb", "/srv/golemdb"] {
            let error = refusal(None, set(unknown));
            assert!(error.contains("use memory"), "{unknown}: {error}");
        }
    }
}
