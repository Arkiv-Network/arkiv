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

use crate::node::ArkivNode;
use arkiv_reth_chainspec::ArkivChainSpecParser;
use reth::CliRunner;
use reth::args::LogArgs;
use reth::chainspec::EthChainSpec;
use reth::providers::{StaticFileProviderFactory, StaticFileSegment};
use reth::tasks::Runtime;
use reth_cli_commands::common::{AccessRights, Environment};
use reth_cli_commands::init_state::InitStateCommand;
use reth_db_common::init::init_from_state_dump;
use reth_storage_api::{BlockNumReader, DatabaseProviderFactory, StorageSettingsCache};
use std::fs::File;
use std::io::BufReader;
use tracing::info;

/// Run the parsed command: tracing and a runtime as reth sets them up for its
/// blocking commands, then [`execute`].
pub fn run(command: InitStateCommand<ArkivChainSpecParser>, mut logs: LogArgs) -> eyre::Result<()> {
    let runner = CliRunner::try_default_runtime()?;
    // Per-network log directory, as reth's CLI does for every command.
    logs.log_file_directory = logs
        .log_file_directory
        .join(command.env.chain.chain().to_string());
    let _guards = logs.init_tracing()?;
    let runtime = runner.runtime();
    runner.run_blocking_until_ctrl_c(execute(command, runtime))
}

/// reth's `InitStateCommand::execute`, with the genesis changesets cleared
/// between opening the datadir and importing.
async fn execute(
    command: InitStateCommand<ArkivChainSpecParser>,
    runtime: Runtime,
) -> eyre::Result<()> {
    if command.without_evm {
        // A snapshot at a later block: reth's path, untouched.
        return command.execute::<ArkivNode>(runtime).await;
    }
    info!(target: "reth::cli", "Reth init-state starting");
    let genesis_alloc_empty = command.env.chain.genesis().alloc.is_empty();
    let Environment {
        config,
        provider_factory,
        ..
    } = command.env.init::<ArkivNode>(AccessRights::RW, runtime)?;
    if genesis_alloc_empty {
        clear_genesis_changesets(&provider_factory)?;
    }

    info!(target: "reth::cli", "Initiating state dump");
    let reader = BufReader::new(File::open(&command.state)?);
    let hash = init_from_state_dump(reader, &provider_factory, config.stages.etl)?;
    info!(target: "reth::cli", hash = ?hash, "Genesis block written");
    Ok(())
}

/// Delete the account and storage changeset static files when they hold
/// nothing but the genesis block's empty entry: a v2-layout datadir at block 0.
fn clear_genesis_changesets<PF>(factory: &PF) -> eyre::Result<()>
where
    PF: DatabaseProviderFactory<Provider: BlockNumReader + StorageSettingsCache>
        + StaticFileProviderFactory,
{
    let provider = factory.database_provider_ro()?;
    // The legacy layout keeps changesets in MDBX, where the importer appends
    // block 0 without complaint; past genesis the files hold real history.
    let at_genesis = provider.last_block_number()? == 0;
    let storage_v2 = provider.cached_storage_settings().storage_v2;
    drop(provider);
    if !(at_genesis && storage_v2) {
        return Ok(());
    }
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
