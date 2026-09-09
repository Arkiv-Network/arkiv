//! `arkiv-reth init-state`, made to work at genesis.
//!
//! reth's `init-state` streams a JSONL state dump into the datadir at the last
//! block it holds — block 0 on a fresh datadir — and checks the rebuilt state
//! root against that block's header. It is the route for a genesis too large
//! to list in `alloc`: a `stateHash` genesis plus a dump, as `arkiv-seed`
//! writes them.
//!
//! In reth v2.5.0 that route is broken at block 0 under the default (v2)
//! storage layout. Opening the datadir writes the genesis block first, which
//! appends block 0 to the account and storage changeset static files — an
//! empty changeset, since the alloc is empty — and the importer then refuses
//! to write block 0's real changesets after it (`trying to append data to
//! AccountChangeSets as block #0 but expected block #1`). The importer's own
//! "reset the changeset files before importing" step explicitly skips block 0,
//! so nothing upstream clears the way.
//!
//! This override runs the same command with one step in between: when the
//! import targets genesis and the genesis has no alloc, it deletes the two
//! changeset segments before importing, so the importer starts them at block 0
//! itself — exactly what it does for a later snapshot block. Everything else
//! is reth's: the same arguments, the same environment setup, the same
//! importer and the same root check. `--without-evm` (a snapshot at a later
//! block) is handed straight to reth.
//!
//! At genesis under the v2 layout the import itself is not reth's either but
//! [`import`]'s: the same tables, changesets and root walk, with an account's
//! storage streamed into its own ETL collector as the line is read, so the
//! dump's largest line — the store's system account, two slots per seeded
//! entity — no longer sets the importer's peak memory. Past genesis, or on the
//! legacy layout, the dump goes to reth's importer as before.
//!
//! One more thing on top: an import of a large dump runs for hours, and reth
//! reports on it only through its log. The override keeps a progress file
//! (`<datadir>/init-state-progress.json`, or `$ARKIV_INIT_STATE_PROGRESS`)
//! rewritten every second, for a watcher to read. See [`progress`].

mod import;
mod progress;

use crate::node::ArkivNode;
use arkiv_reth_chainspec::ArkivChainSpecParser;
use progress::Progress;
use reth::CliRunner;
use reth::args::LogArgs;
use reth::chainspec::EthChainSpec;
use reth::providers::{StaticFileProviderFactory, StaticFileSegment};
use reth::tasks::Runtime;
use reth_cli_commands::common::{AccessRights, Environment};
use reth_cli_commands::init_state::InitStateCommand;
use reth_db_common::init::init_from_state_dump;
use reth_storage_api::{BlockNumReader, DatabaseProviderFactory, StorageSettingsCache};
use reth_tracing::Layers;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

/// Names the progress file; without it the file sits in the datadir.
const PROGRESS_ENV: &str = "ARKIV_INIT_STATE_PROGRESS";
/// The progress file's name in the datadir.
const PROGRESS_FILE: &str = "init-state-progress.json";
/// How often the progress file is rewritten.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// Run the parsed command: tracing and a runtime as reth sets them up for its
/// blocking commands, with the progress layer listening in, then [`execute`].
pub fn run(command: InitStateCommand<ArkivChainSpecParser>, mut logs: LogArgs) -> eyre::Result<()> {
    let runner = CliRunner::try_default_runtime()?;
    // Per-network log directory, as reth's CLI does for every command.
    logs.log_file_directory = logs
        .log_file_directory
        .join(command.env.chain.chain().to_string());
    // The importer reports through its log; the progress file follows it.
    let progress = Progress::new(&command.state)?;
    let mut layers = Layers::new();
    layers.add_layer(progress.layer());
    let _guards = logs.init_tracing_with_layers(layers, false)?;
    let runtime = runner.runtime();
    runner.run_blocking_until_ctrl_c(execute(command, runtime, progress))
}

/// Where the progress file goes: `$ARKIV_INIT_STATE_PROGRESS`, else the
/// datadir.
fn progress_path(command: &InitStateCommand<ArkivChainSpecParser>) -> PathBuf {
    if let Some(path) = std::env::var_os(PROGRESS_ENV) {
        return PathBuf::from(path);
    }
    command
        .env
        .datadir
        .clone()
        .resolve_datadir(command.env.chain.chain())
        .data_dir()
        .join(PROGRESS_FILE)
}

/// reth's `InitStateCommand::execute`, with the genesis changesets cleared
/// between opening the datadir and importing.
async fn execute(
    command: InitStateCommand<ArkivChainSpecParser>,
    runtime: Runtime,
    progress: Progress,
) -> eyre::Result<()> {
    if command.without_evm {
        // A snapshot at a later block: reth's path, untouched.
        return command.execute::<ArkivNode>(runtime).await;
    }
    info!(target: "reth::cli", "Reth init-state starting");
    let progress_path = progress_path(&command);
    info!(target: "arkiv-reth", path = %progress_path.display(), "progress file");
    // Writes until dropped, which is after the outcome is recorded below.
    let _writer = progress.spawn_writer(progress_path, PROGRESS_EVERY);
    match import(command, runtime, &progress) {
        Ok(hash) => {
            progress.finish(hash);
            Ok(())
        }
        Err(e) => {
            progress.fail(&e);
            Err(e)
        }
    }
}

/// Open the datadir, clear the genesis changesets if the import targets
/// genesis, and stream the dump in. Returns the hash of the block written.
fn import(
    command: InitStateCommand<ArkivChainSpecParser>,
    runtime: Runtime,
    progress: &Progress,
) -> eyre::Result<alloy_primitives::B256> {
    let genesis_alloc_empty = command.env.chain.genesis().alloc.is_empty();
    let Environment {
        config,
        provider_factory,
        ..
    } = command.env.init::<ArkivNode>(AccessRights::RW, runtime)?;
    let seeding_genesis_v2 = at_genesis_under_v2(&provider_factory)?;
    if seeding_genesis_v2 && genesis_alloc_empty {
        clear_genesis_changesets(&provider_factory)?;
    }

    info!(target: "reth::cli", "Initiating state dump");
    let reader = progress.reader(BufReader::new(File::open(&command.state)?));
    let hash = if seeding_genesis_v2 {
        import::import_at_genesis(reader, &provider_factory, config.stages.etl)?
    } else {
        init_from_state_dump(reader, &provider_factory, config.stages.etl)?
    };
    info!(target: "reth::cli", hash = ?hash, "Genesis block written");
    Ok(hash)
}

/// Whether the datadir is at block 0 under storage layout v2: the case reth's
/// importer cannot handle and [`import`] takes over. The legacy layout keeps
/// changesets in MDBX, where reth's importer appends block 0 without
/// complaint; past genesis the files hold real history.
fn at_genesis_under_v2<PF>(factory: &PF) -> eyre::Result<bool>
where
    PF: DatabaseProviderFactory<Provider: BlockNumReader + StorageSettingsCache>,
{
    let provider = factory.database_provider_ro()?;
    Ok(provider.last_block_number()? == 0 && provider.cached_storage_settings().storage_v2)
}

/// Delete the account and storage changeset static files, which hold nothing
/// but the genesis block's empty entry.
fn clear_genesis_changesets<PF: StaticFileProviderFactory>(factory: &PF) -> eyre::Result<()> {
    let static_files = factory.static_file_provider();
    for segment in [
        StaticFileSegment::AccountChangeSets,
        StaticFileSegment::StorageChangeSets,
    ] {
        static_files.delete_segment(segment)?;
    }
    info!(
        target: "arkiv-reth",
        "cleared the genesis changeset static files so the state dump imports at block 0"
    );
    Ok(())
}
