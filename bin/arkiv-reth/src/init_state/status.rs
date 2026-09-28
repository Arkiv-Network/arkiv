//! Durable genesis-import status, independent of the advisory progress file.
//!
//! The importer commits state in chunks across MDBX, static files and RocksDB.
//! Record an attempt before touching state, and completion only after all writes
//! and the root check succeed. A crash between those points requires a new
//! datadir. Keeping the record in MDBX also gives it the database's write lock
//! and lifetime; a leftover progress file cannot authorize an empty database.

use alloy_primitives::B256;
use arkiv_reth_chainspec::ArkivChainSpec;
use eyre::{WrapErr, ensure};
use reth::chainspec::EthChainSpec;
use reth::providers::{DBProvider, DatabaseProviderFactory};
use reth_db_api::tables;
use reth_db_api::transaction::{DbTx, DbTxMut};
use serde::{Deserialize, Serialize};

/// A namespaced custom stage's data; reth's normal stages leave this key alone.
const KEY: &str = "ArkivGenesisImport";

#[derive(Debug, Deserialize, Serialize)]
struct Status {
    version: u8,
    genesis_hash: B256,
    state_root: B256,
    complete: bool,
}

/// An empty alloc with a nonempty asserted root needs an external state dump.
pub fn requires_import(chain: &ArkivChainSpec) -> bool {
    chain.genesis().alloc.is_empty()
        && chain.genesis_header().state_root != reth_trie::EMPTY_ROOT_HASH
}

/// Run before starting any node components, including the pruning bootstrap.
pub fn ensure_complete<PF>(factory: &PF, chain: &ArkivChainSpec) -> eyre::Result<()>
where
    PF: DatabaseProviderFactory<Provider: DBProvider>,
{
    if !requires_import(chain) {
        return Ok(());
    }
    let provider = factory.database_provider_ro()?;
    let bytes = provider
        .tx_ref()
        .get::<tables::StageCheckpointProgresses>(KEY.into())?;
    let status = bytes
        .map(|bytes| serde_json::from_slice::<Status>(&bytes))
        .transpose()
        .wrap_err("invalid genesis import status; restore a verified datadir or import into a fresh datadir")?;
    ensure!(
        status.is_some_and(|s| s.version == 1
            && s.complete
            && s.genesis_hash == chain.genesis_hash()
            && s.state_root == chain.genesis_header().state_root),
        "genesis state import is not complete for this chain; run init-state successfully on a fresh datadir before starting the node. Failed, interrupted, or older unrecorded imports require a fresh datadir"
    );
    Ok(())
}

/// Reject retries and preexisting state, including imports made by older binaries
/// which did not leave a status record. Called before changeset deletion.
pub fn ensure_fresh<TX: DbTx>(tx: &TX) -> eyre::Result<()> {
    ensure!(
        tx.get::<tables::StageCheckpointProgresses>(KEY.into())?
            .is_none(),
        "genesis import was already attempted in this datadir; refusing to overwrite it. Use a fresh datadir for another import"
    );
    ensure!(
        tx.entries::<tables::HashedAccounts>()? == 0
            && tx.entries::<tables::HashedStorages>()? == 0
            && tx.entries::<tables::PlainAccountState>()? == 0
            && tx.entries::<tables::PlainStorageState>()? == 0,
        "genesis datadir already contains state; refusing to overwrite it. Use a fresh datadir for another import"
    );
    Ok(())
}

pub fn record<TX: DbTxMut>(tx: &TX, chain: &ArkivChainSpec, complete: bool) -> eyre::Result<()> {
    let status = Status {
        version: 1,
        genesis_hash: chain.genesis_hash(),
        state_root: chain.genesis_header().state_root,
        complete,
    };
    tx.put::<tables::StageCheckpointProgresses>(KEY.into(), serde_json::to_vec(&status)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::ArkivNode;
    use arkiv_reth_chainspec::ArkivChainSpecParser;
    use clap::Parser;
    use reth::CliRunner;
    use reth_cli_commands::common::AccessRights;
    use reth_cli_commands::init_state::InitStateCommand;

    #[test]
    fn interrupted_import_record_survives_reopen_and_cannot_authorize_startup() {
        let dir = tempfile::tempdir().unwrap();
        let genesis = arkiv_seed::export::genesis_with_state_hash(
            arkiv_seed::export::dev_genesis(42),
            B256::repeat_byte(7),
        )
        .unwrap()
        .to_string();
        let command = InitStateCommand::<ArkivChainSpecParser>::parse_from([
            "init-state",
            "--chain",
            &genesis,
            "--datadir",
            dir.path().to_str().unwrap(),
            "unused.jsonl",
        ]);
        let runner = CliRunner::try_default_runtime().unwrap();
        // Commit exactly the durable state left if the importer is killed after
        // recording its attempt. Drop all handles and reopen the real database.
        {
            let env = command
                .env
                .init::<ArkivNode>(AccessRights::RW, runner.runtime())
                .unwrap();
            assert!(ensure_complete(&env.provider_factory, &command.env.chain).is_err());
            let provider = env.provider_factory.database_provider_rw().unwrap();
            ensure_fresh(provider.tx_ref()).unwrap();
            record(provider.tx_ref(), &command.env.chain, false).unwrap();
            provider.commit().unwrap();
        }
        let env = command
            .env
            .init::<ArkivNode>(AccessRights::RW, runner.runtime())
            .unwrap();
        assert!(ensure_complete(&env.provider_factory, &command.env.chain).is_err());
        {
            let provider = env.provider_factory.database_provider_ro().unwrap();
            assert!(ensure_fresh(provider.tx_ref()).is_err());
        }
        // Completed records must still match the version, genesis hash and root.
        let valid = Status {
            version: 1,
            genesis_hash: command.env.chain.genesis_hash(),
            state_root: command.env.chain.genesis_header().state_root,
            complete: true,
        };
        let value = serde_json::to_value(&valid).unwrap();
        for (field, wrong) in [
            ("version", serde_json::json!(2)),
            ("genesis_hash", serde_json::json!(B256::ZERO)),
            ("state_root", serde_json::json!(B256::ZERO)),
            ("complete", serde_json::json!(false)),
        ] {
            let mut invalid = value.clone();
            invalid[field] = wrong;
            let provider = env.provider_factory.database_provider_rw().unwrap();
            provider
                .tx_ref()
                .put::<tables::StageCheckpointProgresses>(
                    KEY.into(),
                    serde_json::to_vec(&invalid).unwrap(),
                )
                .unwrap();
            provider.commit().unwrap();
            assert!(
                ensure_complete(&env.provider_factory, &command.env.chain).is_err(),
                "{field}"
            );
        }
        let provider = env.provider_factory.database_provider_rw().unwrap();
        provider
            .tx_ref()
            .put::<tables::StageCheckpointProgresses>(KEY.into(), b"bad json".to_vec())
            .unwrap();
        provider.commit().unwrap();
        assert!(ensure_complete(&env.provider_factory, &command.env.chain).is_err());
    }

    #[test]
    fn legacy_imported_state_without_a_record_is_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let command = InitStateCommand::<ArkivChainSpecParser>::parse_from([
            "init-state",
            "--datadir",
            dir.path().to_str().unwrap(),
            "unused.jsonl",
        ]);
        let runner = CliRunner::try_default_runtime().unwrap();
        let env = command
            .env
            .init::<ArkivNode>(AccessRights::RW, runner.runtime())
            .unwrap();
        // The ordinary dev alloc already populated state, without our record.
        let provider = env.provider_factory.database_provider_ro().unwrap();
        assert!(ensure_fresh(provider.tx_ref()).is_err());
        // Ordinary genesis never requires an external import record.
        assert!(!requires_import(&command.env.chain));
        ensure_complete(&env.provider_factory, &command.env.chain).unwrap();
    }
}
