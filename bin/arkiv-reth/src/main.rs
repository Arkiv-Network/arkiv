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

mod genesis;
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

use arkiv_reth_chainspec::ArkivChainSpec;
use arkiv_reth_executor::ArkivEvmFactory;
use arkiv_reth_golemdbprovider::{ArkivProvider, ArkivProviderBuilder};
use arkiv_reth_statemanager::{BlockSeals, HostStore};
use clap::Parser;
use clap::builder::RangedU64ValueParser;
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
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// An example for the error messages.
const PATH_EXAMPLE: &str = "ARKIV_STORE_PATH=/var/lib/arkiv/golemdb";

/// Open the store Arkiv's state lives in. There is exactly one, and it holds
/// every entity, the query index and the minting nonces.
///
/// Exactly one of two variables names it:
/// - `ARKIV_STORE_PATH` (`path`) is GolemDB's directory. It must be absolute
///   or start with `~`, the home directory; otherwise it is taken as written.
/// - `ARKIV_EXPERIMENTAL_STORE` (`experimental`) is for tests: `memory` is the
///   in-process reference store, gone when the process is. It does not use the
///   genesis file's GolemDB settings.
///
/// GolemDB is created with the settings of the chain `spec` describes.
fn open_store(
    spec: &ArkivChainSpec,
    path: Option<OsString>,
    experimental: Option<OsString>,
) -> eyre::Result<HostStore> {
    if experimental.is_some() && std::env::var_os("ARKIV_STORE_HISTORY").is_some() {
        warn!(target: "arkiv-reth", "ARKIV_STORE_HISTORY is ignored: only GolemDB prunes");
    }
    match (path, experimental) {
        (Some(path), None) => {
            let path = expand_home(path.into())?;
            eyre::ensure!(
                path.is_absolute(),
                "ARKIV_STORE_PATH={}: the GolemDB directory must be an absolute path, such as \
                 {PATH_EXAMPLE} or ~/arkiv/golemdb",
                path.display()
            );
            open_golemdb(&path, spec)
        }
        (None, Some(store)) => match store.to_str() {
            Some("memory") => {
                info!(target: "arkiv-reth", "Arkiv state in-process, lost when the node stops");
                Ok(Arc::new(arkiv_interfaces::store::reference::MemStore::new()))
            }
            _ => eyre::bail!(
                "ARKIV_EXPERIMENTAL_STORE={}: use memory",
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

/// Open GolemDB in `directory`, creating it with the chain's settings if it is
/// new.
fn open_golemdb(directory: &Path, spec: &ArkivChainSpec) -> eyre::Result<HostStore> {
    let at = directory.display();
    let history = match std::env::var("ARKIV_STORE_HISTORY") {
        Err(std::env::VarError::NotPresent) => History::HeadOnly,
        history => history
            .map_err(|e| eyre::eyre!("ARKIV_STORE_HISTORY: {e}"))?
            .parse()?,
    };
    let config = golemdb_config(spec)?;
    let store = arkiv_golemdb::GolemDbStore::open(directory, &config, history.pruning_config())
        .map_err(|e| match e {
            arkiv_golemdb::OpenError::GenesisMismatch => eyre::eyre!(
                "open GolemDB at {at}: it was created with other GolemDB settings. Delete it if \
                 the node never ran on it; otherwise use a new directory"
            ),
            e => eyre::eyre!("open GolemDB at {at}: {e}"),
        })?;
    info!(
        target: "arkiv-reth",
        directory = %at,
        hash_function = ?config.genesis.hash_function,
        ?history,
        "Arkiv state on GolemDB"
    );
    Ok(Arc::new(store))
}

/// GolemDB's settings for the chain, from its genesis file, with Arkiv's
/// segments. They are fixed when a database is created and part of its state
/// root, so every node of a chain must use the same.
fn golemdb_config(spec: &ArkivChainSpec) -> eyre::Result<arkiv_golemdb::GolemDbConfig> {
    use arkiv_interfaces::constants::{
        MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES,
    };
    use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
    let limit = |bytes: usize| u32::try_from(bytes).expect("Arkiv's limits fit in u32");
    let limits = arkiv_golemdb::CellLimits {
        max_cell_name_len: limit(MAX_ATTRIBUTE_NAME_BYTES),
        max_str_len: limit(MAX_STR_BYTES),
        max_bytes_len: limit(MAX_BYTES_VALUE_LEN),
    };
    let count = |cells: usize| u64::try_from(cells).expect("Arkiv's limits fit in u64");
    let record_limits = arkiv_golemdb::RecordLimits {
        max_cells: count(MAX_RECORD_CELLS),
        max_indexed_cells: count(MAX_RECORD_INDEXED_CELLS),
    };
    let genesis = spec.golemdb().ok_or_else(|| {
        eyre::eyre!(
            "the genesis file has no config.arkiv.golemdb, which a chain on GolemDB needs; \
             `arkiv-reth dump-genesis --chain dev` prints a genesis that has one"
        )
    })?;
    // Arkiv checks names and values against its own limits before GolemDB sees
    // them, so the file must state those.
    eyre::ensure!(
        genesis.cell_limits == limits,
        "genesis config.arkiv.golemdb.cell_limits must be Arkiv's limits, {limits:?}; the file \
         has {:?}",
        genesis.cell_limits
    );
    // And an entity's attributes against its cap, so the record caps must be
    // the ones an entity with every attribute it may have fills: lower, and
    // GolemDB would refuse a write Arkiv accepted.
    eyre::ensure!(
        genesis.record_limits == record_limits,
        "genesis config.arkiv.golemdb.record_limits must be Arkiv's limits, {record_limits:?}; \
         the file has {:?}",
        genesis.record_limits
    );
    Ok(arkiv_golemdb::GolemDbConfig {
        genesis: genesis.clone(),
        segments: arkiv_reth_statemanager::segments::ALL
            .iter()
            .map(|&(name, columns)| arkiv_golemdb::SegmentSpec {
                name: name.to_owned(),
                columns,
            })
            .collect(),
    })
}

/// How much history GolemDB keeps, as `ARKIV_STORE_HISTORY` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum History {
    /// That many past blocks besides the head.
    Blocks(u64),
    /// Every block.
    Archive,
    /// Only the head; the default.
    HeadOnly,
}

impl std::str::FromStr for History {
    type Err = eyre::Report;

    fn from_str(history: &str) -> eyre::Result<Self> {
        Ok(match history {
            "archive" => Self::Archive,
            "head-only" => Self::HeadOnly,
            n => Self::Blocks(n.parse().map_err(|_| {
                eyre::eyre!(
                    "ARKIV_STORE_HISTORY={history}: use an integer number of blocks, 'archive' \
                     or 'head-only'"
                )
            })?),
        })
    }
}

impl History {
    /// GolemDB's pruning for this history; `None` keeps every commit.
    fn pruning_config(self) -> Option<arkiv_golemdb::PruningConfig> {
        match self {
            Self::Blocks(window) => Some(arkiv_golemdb::PruningConfig::new(window)),
            Self::Archive => None,
            Self::HeadOnly => Some(arkiv_golemdb::PruningConfig::new(0)),
        }
    }
}

/// Close gaps by downloading blocks as far as reth allows, unless the
/// operator changed `--engine.backfill-threshold`. A value equal to reth's
/// default counts as unchanged.
///
/// A larger gap runs reth's pipeline, which executes blocks without saving
/// them to GolemDB, so a node on GolemDB cannot catch up that way. reth clamps
/// the threshold to its maximum, the most blocks a peer sends in one answer.
fn download_gaps(engine: &mut reth::args::EngineArgs) {
    if engine.backfill_run_threshold == reth::args::EngineArgs::default().backfill_run_threshold {
        engine.backfill_run_threshold = u64::MAX;
    }
}

/// Arkiv's own `node` flags, next to reth's.
#[derive(Debug, Clone, clap::Args)]
struct ArkivArgs {
    /// Maximum number of concurrent `arkiv_*` requests; further ones wait for a turn.
    ///
    /// Each holds one GolemDB read slot. The whole node shares a fixed number of
    /// them with block import and `eth_*` reads, and with none left reads fail.
    #[arg(
        long = "rpc.max-arkiv-requests",
        value_name = "COUNT",
        value_parser = RangedU64ValueParser::<usize>::new().range(1..=Semaphore::MAX_PERMITS as u64),
        default_value_t = arkiv_reth_rpc::DEFAULT_MAX_CONCURRENT_READS
    )]
    rpc_max_arkiv_requests: usize,
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
    // runtime of its own, and tokio refuses to drop one from async code, which
    // a failed open would.
    let path = std::env::var_os("ARKIV_STORE_PATH");
    let experimental = std::env::var_os("ARKIV_EXPERIMENTAL_STORE");
    // GolemDB is the store whose genesis root block 0's header carries.
    let on_golemdb = path.is_some();
    let store = Arc::new(OnceLock::<HostStore>::new());
    let open = move |spec: &ArkivChainSpec| {
        store
            .get_or_init(|| {
                tokio::task::block_in_place(|| open_store(spec, path.clone(), experimental.clone()))
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
    let mut cli = Cli::<genesis::NodeChainSpecParser, ArkivArgs>::parse();
    if on_golemdb {
        cli.apply_node_command(|node| download_gaps(&mut node.engine));
    }
    let result = cli.run_with_components::<ArkivNode>(
        move |spec| {
            (
                EthEvmConfig::new_with_evm_factory(
                    spec.clone(),
                    ArkivEvmFactory::new(factory_open(&spec), factory_seals.clone()),
                ),
                Arc::new(EthBeaconConsensus::new(spec)),
            )
        },
        async move |builder, arkiv_args| {
            info!(target: "arkiv-reth", "Launching arkiv-reth (reth host + Arkiv entity engine)");
            let store = open(&builder.config().chain);
            // GolemDB keeps the chain's price list from its genesis; a node on
            // it must have a usable one. The in-memory mock has none.
            if on_golemdb {
                let pricing = store.pricing_snapshot(store.head()).map_err(|e| {
                    eyre::eyre!(
                        "the store has no usable pricing model \
                         (config.arkiv.golemdb.metering): {e:?}"
                    )
                })?;
                arkiv_tokenomics::FeeSchedule::from_model(&pricing.model)
                    .map_err(|e| eyre::eyre!("invalid host pricing: {e:?}"))?;
            }
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
                    let module = arkiv_reth_rpc::arkiv_module(
                        ctx.provider().clone(),
                        rpc_store.clone(),
                        arkiv_args.rpc_max_arkiv_requests,
                    )?;
                    ctx.modules.merge_configured(module)?;
                    info!(target: "arkiv-reth", "arkiv_* RPC namespace registered");
                    Ok(())
                });
            let launcher = builder.engine_api_launcher().with_provider_builder(
                ArkivProviderBuilder::new(store.clone(), seals.clone())
                    .check_genesis_root(on_golemdb),
            );
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
    use arkiv_golemdb::HashAlgorithm;
    use serde_json::{Value, json};

    fn set(value: impl Into<OsString>) -> Option<OsString> {
        Some(value.into())
    }

    #[test]
    fn gaps_are_downloaded_as_far_as_reth_allows() {
        let mut engine = reth::args::EngineArgs::default();
        download_gaps(&mut engine);
        let config = engine.tree_config();
        assert_eq!(config.backfill_run_threshold(), 1024);
        // reth raises its block buffer with it, to hold the downloaded blocks.
        assert_eq!(config.block_buffer_limit(), 2048);
    }

    #[test]
    fn a_backfill_threshold_the_operator_set_is_kept() {
        let mut engine = reth::args::EngineArgs {
            backfill_run_threshold: 100,
            ..Default::default()
        };
        download_gaps(&mut engine);
        assert_eq!(engine.tree_config().backfill_run_threshold(), 100);
    }

    /// `--rpc.max-arkiv-requests` as the `node` command parses `flags`.
    fn arkiv_requests(flags: &[&str]) -> Result<usize, clap::Error> {
        let args = ["arkiv-reth", "node"].iter().chain(flags).copied();
        let mut cli = Cli::<genesis::NodeChainSpecParser, ArkivArgs>::try_parse_from(args)?;
        let mut requests = None;
        cli.apply_node_command(|node| requests = Some(node.ext.rpc_max_arkiv_requests));
        Ok(requests.expect("a node command"))
    }

    #[test]
    fn the_arkiv_request_limit_has_a_default_and_refuses_zero() {
        let default = arkiv_reth_rpc::DEFAULT_MAX_CONCURRENT_READS;
        assert_eq!(arkiv_requests(&[]).unwrap(), default);
        assert_eq!(
            arkiv_requests(&["--rpc.max-arkiv-requests", "64"]).unwrap(),
            64
        );
        assert!(arkiv_requests(&["--rpc.max-arkiv-requests", "0"]).is_err());
        let too_many = (Semaphore::MAX_PERMITS + 1).to_string();
        assert!(arkiv_requests(&["--rpc.max-arkiv-requests", &too_many]).is_err());
    }

    /// A chain spec whose genesis has `config.arkiv.golemdb` set to `golemdb`,
    /// or no section.
    fn spec(golemdb: Option<Value>) -> ArkivChainSpec {
        let mut genesis = json!({ "config": { "chainId": 424242 } });
        if let Some(golemdb) = golemdb {
            genesis["config"]["arkiv"] = json!({ "golemdb": golemdb });
        }
        ArkivChainSpec::from_genesis(serde_json::from_value(genesis).unwrap()).unwrap()
    }

    /// Why `open_store` refused, for a call that must not open anything.
    fn refusal(path: Option<OsString>, experimental: Option<OsString>) -> String {
        match open_store(&spec(None), path, experimental) {
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
            open_store(&spec(Some(golemdb("keccak-256"))), set(&path), None).expect(name);
            assert!(path.is_dir(), "{name}");
        }
    }

    /// [`golemdb_config`] for a genesis whose `config.arkiv.golemdb` is
    /// `golemdb`, or that has none.
    fn settings(golemdb: Option<Value>) -> eyre::Result<arkiv_golemdb::GolemDbConfig> {
        golemdb_config(&spec(golemdb))
    }

    /// `config.arkiv.golemdb` with `hash_function`, Arkiv's limits and free
    /// metering prices.
    fn golemdb(hash_function: &str) -> Value {
        let mut genesis: Value =
            serde_json::from_str(include_str!("../../../genesis/example.json")).unwrap();
        let mut golemdb = genesis["config"]["arkiv"]["golemdb"].take();
        golemdb["hash_function"] = json!(hash_function);
        golemdb
    }

    /// `archive` keeps every commit, `head-only` and `0` the head alone, and a
    /// number that many blocks besides; anything else is refused.
    #[test]
    fn history_names_the_pruning_window() {
        let window = |history: &str| {
            let history: History = history.parse().unwrap();
            history.pruning_config().map(|p| p.window())
        };
        assert_eq!(window("archive"), None);
        assert_eq!(window("head-only"), Some(0));
        assert_eq!(window("0"), Some(0));
        assert_eq!(window("100"), Some(100));
        for wrong in ["-1", "", "abc"] {
            let error = wrong.parse::<History>().unwrap_err().to_string();
            assert!(
                error.contains("use an integer number of blocks"),
                "{wrong}: {error}"
            );
        }
    }

    /// GolemDB has no settings of its own: they come from the genesis file.
    #[test]
    fn golemdb_needs_the_genesis_settings() {
        let error = settings(None).unwrap_err().to_string();
        assert!(error.contains("no config.arkiv.golemdb"), "{error}");
    }

    #[test]
    fn golemdb_settings_follow_the_genesis_file() {
        for (name, hash) in [
            ("keccak-256", HashAlgorithm::Keccak256),
            ("blake3", HashAlgorithm::Blake3),
        ] {
            assert_eq!(
                settings(Some(golemdb(name))).unwrap().genesis.hash_function,
                hash
            );
        }
    }

    /// The repository's example genesis file, read as `--chain` reads a file,
    /// passes the node's checks.
    #[test]
    fn the_example_genesis_file_is_accepted() {
        use reth_cli::chainspec::ChainSpecParser;
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../genesis/example.json");
        let spec = genesis::NodeChainSpecParser::parse(path).unwrap();
        assert!(spec.golemdb().is_some(), "it sets config.arkiv.golemdb");
        assert_eq!(
            golemdb_config(&spec).unwrap().genesis.hash_function,
            HashAlgorithm::Keccak256
        );
    }

    /// Every weight of the example genesis file reaches the store the node
    /// opens on it, under its own name, and every host weight the field of the
    /// fee schedule it names. The file's values all differ, so one that lands
    /// under another name, or is left at a default, shows.
    #[test]
    fn every_weight_of_the_example_genesis_file_reaches_the_node() {
        use arkiv_tokenomics::FeeSchedule;
        use reth_cli::chainspec::ChainSpecParser;
        use std::collections::{BTreeMap, BTreeSet};

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../genesis/example.json");
        let file: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let metering = &file["config"]["arkiv"]["golemdb"]["metering"];
        let weights: BTreeMap<String, u64> =
            serde_json::from_value(metering["weights"].clone()).unwrap();
        let distinct: BTreeSet<u64> = weights.values().copied().collect();
        assert_eq!(
            distinct.len(),
            weights.len(),
            "the example's weights must all differ"
        );

        let spec = genesis::NodeChainSpecParser::parse(path).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let store = open_store(&spec, set(dir.path().join("golemdb")), None).unwrap();
        let model = store.pricing_snapshot(store.head()).unwrap().model;
        assert_eq!(*model.weights, weights);
        assert_eq!(model.golemdb_code_version, metering["golemdb_code_version"]);
        assert_eq!(model.host_code_version, metering["host_code_version"]);

        let schedule = FeeSchedule::from_model(&model).unwrap();
        let host = |name: &str| weights[&format!("host.arkiv.{name}")];
        assert_eq!(schedule.capacity_rate, host("capacity_rate"));
        assert_eq!(schedule.cell_byte_units, host("cell_byte_units"));
        assert_eq!(schedule.index_byte_units, host("index_byte_units"));
        assert_eq!(schedule.c_cell, host("c_cell"));
        assert_eq!(schedule.c_idx, host("c_idx"));
        assert_eq!(schedule.max_lifetime, host("max_lifetime"));
    }

    #[test]
    fn limits_other_than_arkivs_are_refused() {
        for (section, name) in [
            ("cell_limits", "max_cell_name_len"),
            ("cell_limits", "max_str_len"),
            ("cell_limits", "max_bytes_len"),
            ("record_limits", "max_cells"),
            ("record_limits", "max_indexed_cells"),
        ] {
            for delta in [-1, 1] {
                let mut other = golemdb("keccak-256");
                let limit = &mut other[section][name];
                *limit = json!(limit.as_i64().unwrap() + delta);
                let error = settings(Some(other)).unwrap_err().to_string();
                assert!(
                    error.contains(&format!("{section} must be Arkiv's limits")),
                    "{section}.{name}: {error}"
                );
            }
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
    fn the_experimental_store_is_memory() {
        assert!(open_store(&spec(None), None, set("memory")).is_ok());
    }

    /// A typo is not taken for some other store.
    #[test]
    fn an_unknown_experimental_store_is_refused() {
        for unknown in [
            "",
            "Memory",
            "redis://127.0.0.1:6379",
            "golemdb",
            "/srv/golemdb",
        ] {
            let error = refusal(None, set(unknown));
            assert!(error.contains("use memory"), "{unknown}: {error}");
        }
    }
}
