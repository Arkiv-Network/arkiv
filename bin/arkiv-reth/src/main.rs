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
use std::env::VarError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::info;
use url::Url;

/// The store Arkiv's state lives in, as `ARKIV_STORE_URL` names it. There is
/// exactly one, and it holds every entity, the query index and the minting
/// nonces.
#[derive(Debug, PartialEq, Eq)]
enum StoreUrl {
    /// `golemdb://` and an absolute path: GolemDB, in-process, in that directory.
    GolemDb(PathBuf),
    /// `memory://`: the in-process reference store, gone when the process is.
    Memory,
    /// Any other URL: a Valkey server.
    Valkey(String),
}

/// An example for the error messages.
const GOLEMDB_EXAMPLE: &str = "golemdb:///var/lib/arkiv/golemdb";

/// Check `ARKIV_STORE_URL` (`text`) before anything opens. GolemDB is the store;
/// the others are for tests and experiments, and run only when
/// `ALLOW_EXPERIMENTAL_BACKENDS` (`allow`) is `true`. Any other URL is refused,
/// so a typo is reported as one.
///
/// A GolemDB URL's path is read as a `file://` URL's is: absolute and
/// percent-decoded. Nothing expands `~`.
fn store_url(text: Result<String, VarError>, allow: Option<&str>) -> eyre::Result<StoreUrl> {
    let text = match text {
        Ok(text) if !text.is_empty() => text,
        Ok(_) => eyre::bail!(
            "ARKIV_STORE_URL is empty; set it to a GolemDB directory, such as {GOLEMDB_EXAMPLE}"
        ),
        Err(VarError::NotPresent) => eyre::bail!(
            "ARKIV_STORE_URL is not set; set it to a GolemDB directory, such as {GOLEMDB_EXAMPLE}"
        ),
        Err(VarError::NotUnicode(_)) => eyre::bail!("ARKIV_STORE_URL is not valid UTF-8"),
    };
    let unknown = || {
        eyre::eyre!(
            "ARKIV_STORE_URL={text} is not a store URL; set it to a GolemDB directory, such as \
             {GOLEMDB_EXAMPLE}"
        )
    };
    let url = Url::parse(&text).map_err(|_| unknown())?;
    let store = match url.scheme() {
        "golemdb" => {
            let path = url.to_file_path().map_err(|()| {
                eyre::eyre!(
                    "ARKIV_STORE_URL={text}: the GolemDB directory must be an absolute path, \
                     such as {GOLEMDB_EXAMPLE} ($HOME works, ~ does not)"
                )
            })?;
            return Ok(StoreUrl::GolemDb(path));
        }
        "memory" if text == "memory://" => StoreUrl::Memory,
        // Every scheme Valkey's client takes: plain, TLS, cluster, sentinel, socket.
        scheme if scheme.starts_with("redis") || scheme.starts_with("valkey") => {
            StoreUrl::Valkey(text.clone())
        }
        _ => return Err(unknown()),
    };
    eyre::ensure!(
        allow == Some("true"),
        "ARKIV_STORE_URL={text} is not GolemDB; set ALLOW_EXPERIMENTAL_BACKENDS=true to run on it"
    );
    Ok(store)
}

/// Open the store `ARKIV_STORE_URL` names.
fn open_store() -> eyre::Result<HostStore> {
    let allow = std::env::var("ALLOW_EXPERIMENTAL_BACKENDS").ok();
    let store: HostStore = match store_url(std::env::var("ARKIV_STORE_URL"), allow.as_deref())? {
        StoreUrl::GolemDb(directory) => open_golemdb(&directory)?,
        StoreUrl::Memory => {
            info!(target: "arkiv-reth", "Arkiv state in-process, lost when the node stops");
            Arc::new(arkiv_interfaces::store::reference::MemStore::new())
        }
        StoreUrl::Valkey(url) => {
            let namespace =
                std::env::var("ARKIV_STORE_NAMESPACE").unwrap_or_else(|_| "arkiv".to_owned());
            let store = arkiv_valkey::ValkeyStore::connect(&url, &namespace)
                .map_err(|e| eyre::eyre!("connect to the Arkiv store at {url}: {e:?}"))?;
            info!(target: "arkiv-reth", %url, %namespace, "Arkiv state on Valkey");
            Arc::new(store)
        }
    };
    Ok(store)
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
    //
    // It opens in `block_in_place`, outside async code: a store may own a
    // runtime of its own, as Valkey's does, and tokio refuses to drop one from
    // async code, which a failed connect does.
    let store = Arc::new(OnceLock::<HostStore>::new());
    let open = move || {
        store
            .get_or_init(|| {
                tokio::task::block_in_place(open_store).unwrap_or_else(|err| {
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

    fn url(value: &str) -> Result<String, VarError> {
        Ok(value.to_owned())
    }

    #[test]
    fn golemdb_needs_no_allow_but_an_absolute_path() {
        assert_eq!(
            store_url(url("golemdb:///srv/golemdb"), None).unwrap(),
            StoreUrl::GolemDb("/srv/golemdb".into())
        );
        assert_eq!(
            store_url(url("golemdb:///srv/my%20dir"), None).unwrap(),
            StoreUrl::GolemDb("/srv/my dir".into())
        );
        for relative in [
            "golemdb://",
            "golemdb://golemdb",
            "golemdb://~/x",
            "golemdb://srv/x",
        ] {
            assert!(
                store_url(url(relative), Some("true")).is_err(),
                "{relative}"
            );
        }
    }

    #[test]
    fn an_unset_or_empty_url_is_refused() {
        assert!(store_url(Err(VarError::NotPresent), Some("true")).is_err());
        assert!(store_url(url(""), Some("true")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_url_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let not_utf8 = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(store_url(Err(VarError::NotUnicode(not_utf8)), Some("true")).is_err());
    }

    #[test]
    fn other_stores_need_the_allow_variable() {
        for allow in [None, Some("1"), Some("TRUE")] {
            assert!(store_url(url("memory://"), allow).is_err(), "{allow:?}");
            assert!(
                store_url(url("redis://127.0.0.1:6379"), allow).is_err(),
                "{allow:?}"
            );
        }
        assert_eq!(
            store_url(url("memory://"), Some("true")).unwrap(),
            StoreUrl::Memory
        );
        for valkey in ["redis://127.0.0.1:6379", "rediss://host", "valkey://host"] {
            assert_eq!(
                store_url(url(valkey), Some("true")).unwrap(),
                StoreUrl::Valkey(valkey.into())
            );
        }
    }

    /// A typo is not taken for some other store, even when others are allowed.
    #[test]
    fn an_unknown_url_is_refused() {
        for unknown in ["golem:///srv/golemdb", "memory://localhost", "/srv/golemdb"] {
            let error = store_url(url(unknown), Some("true")).unwrap_err();
            assert!(
                error.to_string().contains("not a store URL"),
                "{unknown}: {error}"
            );
        }
    }
}
