//! The state-dump importer, with an account's storage streamed past memory.
//!
//! reth's importer (`init_from_state_dump`) reads the dump a line at a time
//! and parses each line into a `GenesisAccount` — its storage as a `BTreeMap`
//! — before handing it to the ETL collector. That holds one account's whole
//! storage in memory three times over (the line, the map, the collector's
//! encoding), so the importer's peak is set by the dump's largest line. In a
//! seeded Arkiv genesis that line is the store's system account, which keeps
//! two slots per entity: at 50 M entities it is 14 GB of JSON, and parsing it
//! took reth past a 32 GB memory limit.
//!
//! This importer does what reth's does — the same tables, changesets and
//! history, the same root walk, the same checks against the header — with the
//! storage taken apart from the account as the line is read. A second ETL
//! collector, keyed by `(address, slot)`, takes the slots one at a time from a
//! streaming parse; the write phase runs the accounts and then the slots
//! through the database in committed chunks. Memory is then bounded by the
//! collectors' buffers whatever the shape of the dump.
//!
//! Only a line longer than [`STREAM_ABOVE`] is parsed as it is read; every
//! other line — every entity record — is parsed from one buffer as reth does.
//! The dump is reth's format: a `{"root": …}` line, then one account per line,
//! each with its `address`. Storage may come before the address (geth's dumps
//! put the address last); such storage is held until the address arrives.
//!
//! Storage layout v2 at genesis only, which is where the override runs; see
//! the parent module.

use alloy_consensus::BlockHeader;
use alloy_genesis::GenesisAccount;
use alloy_primitives::map::B256Set;
use alloy_primitives::{Address, B256, U64, U256, keccak256};
use eyre::{WrapErr, bail, ensure, eyre};
use reth::primitives::{Account, Bytecode, NodePrimitives, StorageEntry};
use reth::providers::providers::{RocksDBBatch, StaticFileProviderRWRefMut, StaticFileWriter};
use reth::providers::{
    BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    NodePrimitivesProvider, RocksDBProviderFactory, StageCheckpointWriter,
    StaticFileProviderFactory, StaticFileSegment, StorageSettingsCache, TrieWriter,
};
use reth_config::config::EtlConfig;
use reth_db_api::cursor::DbCursorRW;
use reth_db_api::models::storage_sharded_key::StorageShardedKey;
use reth_db_api::models::{
    AccountBeforeTx, AddressStorageKey, CompactU256, IntegerList, ShardedKey, StorageBeforeTx,
};
use reth_db_api::table::{Decode, Decompress};
use reth_db_api::tables;
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_etl::Collector;
use reth_stages_types::{StageCheckpoint, StageId};
use reth_trie::{IntermediateStateRootState, StateRoot, StateRootProgress};
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory, TrieTableAdapter,
};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read};
use tracing::{error, info};

/// The log target reth's importer reports under, and the progress layer
/// listens to.
const TARGET: &str = "reth::cli";

/// A line up to this long is parsed from one buffer, as reth parses every
/// line; a longer one is parsed as it is read. Every entity record is far
/// below it; the system account's line is far above it at any seed worth
/// streaming, and the few hundred shared index buckets in between cost a
/// second either way.
const STREAM_ABOVE: usize = 64 * 1024;

/// The database is committed every this many rows written — accounts, then
/// slots — to bound MDBX's dirty pages, as reth's importer commits per
/// storage unit.
const COMMIT_EVERY: usize = 100_000;

/// Accounts, one per line, sorted by address.
type Accounts = Collector<Address, GenesisAccount>;
/// Storage slots, sorted by `(address, slot)`.
type Slots = Collector<AddressStorageKey, CompactU256>;

/// Import the dump behind `reader` at the genesis block of `factory`'s
/// datadir and check the rebuilt state root against the genesis header's.
/// Returns the genesis block's hash.
pub fn import_at_genesis<PF>(
    mut reader: impl BufRead,
    factory: &PF,
    etl: EtlConfig,
) -> eyre::Result<B256>
where
    PF: StaticFileProviderFactory
        + DatabaseProviderFactory<
            ProviderRW: DBProvider<Tx: DbTxMut>
                            + BlockNumReader
                            + BlockHashReader
                            + HeaderProvider
                            + StageCheckpointWriter
                            + StaticFileProviderFactory
                            + RocksDBProviderFactory
                            + NodePrimitivesProvider
                            + StorageSettingsCache
                            + TrieWriter,
        >,
{
    ensure!(etl.file_size > 0, "ETL file size cannot be zero");
    let (block, hash, expected_root) = {
        let provider = factory.database_provider_rw()?;
        let block = provider.last_block_number()?;
        ensure!(
            block == 0,
            "the datadir is at block {block}; this importer only seeds genesis"
        );
        let hash = provider
            .block_hash(block)?
            .ok_or_else(|| eyre!("block hash not found for block {block}"))?;
        let header = provider
            .header_by_number(block)?
            .ok_or_else(|| eyre!("header not found for block {block}"))?;
        (block, hash, header.state_root())
    };

    validate_dump_root(&mut reader, expected_root)?;

    let (accounts, slots) = parse(&mut reader, etl)?;
    // The caller has rejected existing state and recorded an import attempt.
    // Parsing failures leave even the empty genesis changesets intact.
    super::clear_genesis_changesets(factory)?;
    write(factory, block, accounts, slots)?;

    info!(target: TARGET, "All accounts written to database, starting state root computation (may take some time)");
    {
        let provider = factory.database_provider_rw()?;
        provider.tx_ref().clear::<tables::AccountsTrie>()?;
        provider.tx_ref().clear::<tables::StoragesTrie>()?;
        provider.commit()?;
    }
    let computed_root = compute_state_root(factory)?;
    if computed_root == expected_root {
        info!(target: TARGET, ?computed_root, "Computed state root matches state root in state dump");
    } else {
        error!(target: TARGET,
            ?computed_root,
            ?expected_root,
            "Computed state root does not match state root in state dump"
        );
        bail!(
            "computed state root {computed_root} does not match {expected_root} in the dump and the genesis header"
        );
    }

    // The stages that need state are done for this block.
    let provider = factory.database_provider_rw()?;
    for stage in StageId::STATE_REQUIRED {
        provider.save_stage_checkpoint(stage, StageCheckpoint::new(block))?;
    }
    provider.commit()?;
    Ok(hash)
}

/// Check the dump's first line before any destructive work.
pub(super) fn validate_dump_root(
    mut reader: impl BufRead,
    expected_root: B256,
) -> eyre::Result<()> {
    let mut first = String::new();
    reader.read_line(&mut first)?;
    #[derive(Deserialize)]
    struct DumpRoot {
        root: B256,
    }
    let dump_root = serde_json::from_str::<DumpRoot>(&first)
        .wrap_err("the dump does not start with its state root")?
        .root;
    ensure!(
        dump_root == expected_root,
        "state root {dump_root} in the dump does not match {expected_root} in the genesis header"
    );
    Ok(())
}

/// Read every account line into the two collectors.
fn parse(mut reader: impl BufRead, etl: EtlConfig) -> eyre::Result<(Accounts, Slots)> {
    let mut accounts = Accounts::new(etl.file_size, etl.dir.clone());
    let mut slots = Slots::new(etl.file_size, etl.dir);
    let mut parsed_accounts = 0u64;
    let mut parsed_slots = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = (&mut reader)
            .take(STREAM_ABOVE as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        let seed = Line {
            accounts: &mut accounts,
            slots: &mut slots,
            parsed_slots: &mut parsed_slots,
        };
        if n < STREAM_ABOVE || line.last() == Some(&b'\n') {
            // The whole line, or the file's last line without a newline.
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let mut de = serde_json::Deserializer::from_slice(&line);
            seed.deserialize(&mut de)?;
            de.end()?;
        } else {
            // The line goes on past the buffer: parse it as it comes.
            let rest = BufReader::with_capacity(
                STREAM_ABOVE,
                line.as_slice().chain(RestOfLine::new(&mut reader)),
            );
            let mut de = serde_json::Deserializer::from_reader(rest);
            seed.deserialize(&mut de)?;
            de.end()?;
        }
        parsed_accounts += 1;
        if parsed_accounts.is_multiple_of(100_000) {
            info!(target: TARGET, parsed_accounts, parsed_slots, "Parsed accounts");
        }
    }
    info!(target: TARGET, parsed_accounts, parsed_slots, "Parsed the state dump");
    Ok((accounts, slots))
}

/// The bytes of a reader up to its next newline, which is consumed but not
/// returned; end of file after that.
struct RestOfLine<'r, R> {
    inner: &'r mut R,
    done: bool,
}

impl<'r, R: BufRead> RestOfLine<'r, R> {
    fn new(inner: &'r mut R) -> Self {
        Self { inner, done: false }
    }
}

impl<R: BufRead> Read for RestOfLine<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        let available = self.inner.fill_buf()?;
        let eof = available.is_empty();
        let window = &available[..available.len().min(out.len())];
        let (n, newline) = match window.iter().position(|&b| b == b'\n') {
            Some(at) => (at, true),
            None => (window.len(), false),
        };
        out[..n].copy_from_slice(&window[..n]);
        self.inner.consume(n + usize::from(newline));
        self.done = newline || eof;
        Ok(n)
    }
}

/// One account line, deserialized straight into the collectors: the account
/// without its storage into one, each slot into the other.
struct Line<'c> {
    accounts: &'c mut Accounts,
    slots: &'c mut Slots,
    parsed_slots: &'c mut u64,
}

/// The fields of an account line. reth's dump has the first five; geth's has
/// more (`root`, `codeHash`, `key`), which are skipped.
#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "camelCase")]
enum Field {
    Address,
    Nonce,
    Balance,
    Code,
    Storage,
    #[serde(other)]
    Other,
}

impl<'de> DeserializeSeed<'de> for Line<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Line<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an account object with its address")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        let Self {
            accounts,
            slots,
            parsed_slots,
        } = self;
        let mut address: Option<Address> = None;
        let mut account = GenesisAccount::default();
        // Storage met before the address has nowhere to go yet.
        let mut early: Option<BTreeMap<B256, B256>> = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Address => address = Some(map.next_value()?),
                Field::Nonce => {
                    account.nonce = map.next_value::<Option<U64>>()?.map(|n| n.to::<u64>());
                }
                Field::Balance => account.balance = map.next_value()?,
                Field::Code => account.code = map.next_value()?,
                Field::Storage => match address {
                    Some(address) => map.next_value_seed(StorageOf {
                        address,
                        slots: &mut *slots,
                        parsed_slots: &mut *parsed_slots,
                    })?,
                    None => early = map.next_value()?,
                },
                Field::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let address = address.ok_or_else(|| de::Error::missing_field("address"))?;
        for (slot, value) in early.into_iter().flatten() {
            insert_slot(slots, address, slot, value).map_err(de::Error::custom)?;
            *parsed_slots += 1;
        }
        accounts.insert(address, account).map_err(de::Error::custom)
    }
}

/// An account's `storage` map, each slot going to the collector as it is read.
struct StorageOf<'c> {
    address: Address,
    slots: &'c mut Slots,
    parsed_slots: &'c mut u64,
}

impl<'de> DeserializeSeed<'de> for StorageOf<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for StorageOf<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a map of storage slots, or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        while let Some((slot, value)) = map.next_entry::<B256, B256>()? {
            insert_slot(self.slots, self.address, slot, value).map_err(de::Error::custom)?;
            *self.parsed_slots += 1;
        }
        Ok(())
    }
}

fn insert_slot(slots: &mut Slots, address: Address, slot: B256, value: B256) -> io::Result<()> {
    slots.insert(
        AddressStorageKey((address, slot)),
        CompactU256(U256::from_be_bytes(value.0)),
    )
}

/// Write the accounts, then the slots, to the database as reth's importer
/// does for storage layout v2: hashed state in MDBX, changesets in static
/// files, history in RocksDB. Committed every [`COMMIT_EVERY`] rows.
fn write<PF>(factory: &PF, block: u64, mut accounts: Accounts, mut slots: Slots) -> eyre::Result<()>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut>
                        + StaticFileProviderFactory
                        + RocksDBProviderFactory
                        + NodePrimitivesProvider,
    >,
{
    let accounts_len = accounts.len() as u64;
    let slots_len = slots.len() as u64;
    info!(target: TARGET, accounts_len, slots_len, "Writing the state");
    // Every history entry is the same one-block list.
    let history_list = IntegerList::new([block])?;

    let provider = factory.database_provider_rw()?;
    let static_files = provider.static_file_provider();
    let rocksdb = provider.rocksdb_provider();
    drop(provider);
    let mut account_changesets =
        static_files.get_writer(block, StaticFileSegment::AccountChangeSets)?;
    let mut storage_changesets =
        static_files.get_writer(block, StaticFileSegment::StorageChangeSets)?;
    for (writer, segment) in [
        (&account_changesets, "account"),
        (&storage_changesets, "storage"),
    ] {
        ensure!(
            writer.next_block_number() == block,
            "the {segment} changeset static files already hold block {block}; is the datadir fresh?"
        );
    }
    account_changesets.begin_account_changeset(block)?;
    storage_changesets.begin_storage_changeset(block)?;

    // Accounts, in address order.
    let mut seen_bytecodes = B256Set::default();
    let mut total_accounts = 0u64;
    let mut entries = accounts.iter()?;
    loop {
        let provider = factory.database_provider_rw()?;
        let mut history = rocksdb.batch_with_auto_commit();
        let mut in_chunk = 0u64;
        for entry in entries.by_ref().take(COMMIT_EVERY) {
            let (address, account) = entry?;
            let address = Address::decode(&address)?;
            let account = GenesisAccount::decompress(&account).map_err(|e| eyre!("{e}"))?;
            write_account(
                provider.tx_ref(),
                &mut account_changesets,
                &mut history,
                address,
                &account,
                &history_list,
                &mut seen_bytecodes,
            )?;
            in_chunk += 1;
        }
        if in_chunk == 0 {
            break;
        }
        total_accounts += in_chunk;
        history.commit()?;
        commit_mdbx_only(provider)?;
        // Bytecodes written so far are in the database; forget them to keep
        // the set bounded, as reth does.
        seen_bytecodes.clear();
        info!(target: TARGET, total_accounts, accounts_len, "Committed chunk");
    }
    drop(entries);

    // Slots, in (address, slot) order — the order the changesets need.
    let mut total_slots = 0u64;
    let mut hashed_address: Option<(Address, B256)> = None;
    let mut entries = slots.iter()?;
    loop {
        let provider = factory.database_provider_rw()?;
        let mut hashed_storage = provider
            .tx_ref()
            .cursor_dup_write::<tables::HashedStorages>()?;
        let mut history = rocksdb.batch_with_auto_commit();
        let mut in_chunk = 0u64;
        for entry in entries.by_ref().take(COMMIT_EVERY) {
            let (key, value) = entry?;
            let AddressStorageKey((address, slot)) = AddressStorageKey::decode(&key)?;
            let value = CompactU256::decompress(&value).map_err(|e| eyre!("{e}"))?.0;
            let hashed = match hashed_address {
                Some((last, hashed)) if last == address => hashed,
                _ => {
                    let hashed = keccak256(address);
                    hashed_address = Some((address, hashed));
                    hashed
                }
            };
            hashed_storage.upsert(
                hashed,
                &StorageEntry {
                    key: keccak256(slot),
                    value,
                },
            )?;
            storage_changesets.append_storage_changeset_entry(StorageBeforeTx {
                address,
                key: slot,
                value: U256::ZERO,
            })?;
            history.put::<tables::StoragesHistory>(
                StorageShardedKey::new(address, slot, u64::MAX),
                &history_list,
            )?;
            in_chunk += 1;
        }
        drop(hashed_storage);
        if in_chunk == 0 {
            break;
        }
        total_slots += in_chunk;
        history.commit()?;
        commit_mdbx_only(provider)?;
        info!(target: TARGET, total_slots, slots_len, "Committed storage chunk");
    }
    drop(entries);

    drop((account_changesets, storage_changesets));
    static_files.finalize()?;
    info!(target: TARGET, total_accounts, total_slots, "All accounts written to database");
    Ok(())
}

/// One account's row in every table it belongs to, storage aside.
fn write_account<TX: DbTxMut, N: NodePrimitives>(
    tx: &TX,
    account_changesets: &mut StaticFileProviderRWRefMut<'_, N>,
    history: &mut RocksDBBatch<'_>,
    address: Address,
    genesis: &GenesisAccount,
    history_list: &IntegerList,
    seen_bytecodes: &mut B256Set,
) -> eyre::Result<()> {
    let bytecode_hash = match &genesis.code {
        Some(code) => {
            let bytecode = Bytecode::new_raw_checked(code.clone())
                .map_err(|e| eyre!("invalid bytecode for {address}: {e}"))?;
            let hash = bytecode.hash_slow();
            if seen_bytecodes.insert(hash) {
                tx.put::<tables::Bytecodes>(hash, bytecode)?;
            }
            Some(hash)
        }
        None => None,
    };
    let account = Account {
        nonce: genesis.nonce.unwrap_or_default(),
        balance: genesis.balance,
        bytecode_hash,
    };
    tx.put::<tables::HashedAccounts>(keccak256(address), account)?;
    account_changesets.append_account_changeset_entry(AccountBeforeTx {
        address,
        info: None,
    })?;
    history.put::<tables::AccountsHistory>(ShardedKey::new(address, u64::MAX), history_list)?;
    Ok(())
}

/// Commit the MDBX transaction alone: the static-file writers stay open
/// across chunks and are finalized once at the end.
fn commit_mdbx_only<P: DBProvider<Tx: DbTxMut>>(provider: P) -> eyre::Result<()> {
    DbTx::commit(provider.into_tx())?;
    Ok(())
}

/// The state root from the hashed tables, flushing trie updates and
/// committing as the walk goes, as reth's importer does.
fn compute_state_root<PF>(factory: &PF) -> eyre::Result<B256>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
    >,
{
    let provider = factory.database_provider_rw()?;
    reth_trie_db::with_adapter!(&provider, |A| {
        drop(provider);
        compute_state_root_with::<PF, A>(factory)
    })
}

fn compute_state_root_with<PF, A>(factory: &PF) -> eyre::Result<B256>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
    >,
    A: TrieTableAdapter,
{
    type DbStateRoot<'a, TX, A> =
        StateRoot<DatabaseTrieCursorFactory<&'a TX, A>, DatabaseHashedCursorFactory<&'a TX>>;

    let mut intermediate: Option<IntermediateStateRootState> = None;
    let mut total_flushed_updates = 0usize;
    loop {
        let provider = factory.database_provider_rw()?;
        let tx = provider.tx_ref();
        let walk = DbStateRoot::<_, A>::from_tx(tx).with_intermediate_state(intermediate.take());
        match walk.root_with_progress()? {
            StateRootProgress::Progress(state, _, updates) => {
                let updated_len = provider.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;
                info!(target: TARGET,
                    last_account_key = %state.account_root_state.last_hashed_key,
                    updated_len,
                    total_flushed_updates,
                    "Flushing trie updates (committing to free memory)"
                );
                intermediate = Some(*state);
                provider.commit()?;
            }
            StateRootProgress::Complete(root, _, updates) => {
                let updated_len = provider.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;
                info!(target: TARGET,
                    %root,
                    updated_len,
                    total_flushed_updates,
                    "State root computation complete"
                );
                provider.commit()?;
                return Ok(root);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address};
    use std::io::Cursor;

    fn etl() -> EtlConfig {
        EtlConfig {
            dir: Some(std::env::temp_dir().join(format!(
                "arkiv-init-state-test-{}-{}",
                std::process::id(),
                rand_suffix()
            ))),
            file_size: 4096,
        }
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    type Drained = (Vec<(Address, GenesisAccount)>, Vec<(Address, B256, U256)>);

    /// Read the collectors back, decoded.
    fn drain((mut accounts, mut slots): (Accounts, Slots)) -> Drained {
        let accounts = accounts
            .iter()
            .unwrap()
            .map(|entry| {
                let (address, account) = entry.unwrap();
                (
                    Address::decode(&address).unwrap(),
                    GenesisAccount::decompress(&account).unwrap(),
                )
            })
            .collect();
        let slots = slots
            .iter()
            .unwrap()
            .map(|entry| {
                let (key, value) = entry.unwrap();
                let AddressStorageKey((address, slot)) = AddressStorageKey::decode(&key).unwrap();
                (address, slot, CompactU256::decompress(&value).unwrap().0)
            })
            .collect();
        (accounts, slots)
    }

    const A: Address = address!("0x00000000000000000000000000000000000000aa");
    const B: Address = address!("0x00000000000000000000000000000000000000bb");
    const C: Address = address!("0x00000000000000000000000000000000000000cc");

    fn slot(i: u64) -> B256 {
        B256::from(U256::from(i))
    }

    #[test]
    fn rest_of_line_stops_at_the_newline_and_leaves_the_rest() {
        let mut reader = Cursor::new(b"tail of line\nnext line\n".to_vec());
        let mut out = String::new();
        RestOfLine::new(&mut reader)
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "tail of line");
        let mut next = String::new();
        reader.read_line(&mut next).unwrap();
        assert_eq!(next, "next line\n");

        // Byte-sized reads, as serde makes them, land on the same boundary.
        let mut reader = Cursor::new(b"ab\ncd".to_vec());
        let mut rest = RestOfLine::new(&mut reader);
        let mut byte = [0u8; 1];
        let mut got = Vec::new();
        loop {
            match rest.read(&mut byte).unwrap() {
                0 => break,
                _ => got.push(byte[0]),
            }
        }
        assert_eq!(got, b"ab");
        assert_eq!(reader.position(), 3);

        // Without a newline, the rest of the file.
        let mut reader = Cursor::new(b"no newline".to_vec());
        let mut out = String::new();
        RestOfLine::new(&mut reader)
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "no newline");
    }

    #[test]
    fn accounts_and_slots_part_ways_whatever_the_field_order() {
        let dump = format!(
            concat!(
                r#"{{"address":"{a}","nonce":"0x1","balance":"0x0","code":"0x6001"}}"#,
                "\n",
                // geth's order: storage first, address last; a blank line after.
                r#"{{"storage":{{"{s2}":"{v2}","{s1}":"{v1}"}},"balance":"0x2a","key":"ignored","address":"{c}"}}"#,
                "\n\n",
                r#"{{"address":"{b}","nonce":"0x3","balance":"0x0","storage":null}}"#,
                "\n",
            ),
            a = A,
            b = B,
            c = C,
            s1 = slot(1),
            v1 = slot(0x11),
            s2 = slot(2),
            v2 = slot(0x22),
        );
        let (accounts, slots) = drain(parse(Cursor::new(dump), etl()).unwrap());
        assert_eq!(
            accounts,
            vec![
                (
                    A,
                    GenesisAccount {
                        nonce: Some(1),
                        code: Some(Bytes::from_static(&[0x60, 0x01])),
                        ..Default::default()
                    }
                ),
                (
                    B,
                    GenesisAccount {
                        nonce: Some(3),
                        ..Default::default()
                    }
                ),
                (
                    C,
                    GenesisAccount {
                        balance: U256::from(42),
                        ..Default::default()
                    }
                ),
            ]
        );
        assert_eq!(
            slots,
            vec![
                (C, slot(1), U256::from(0x11)),
                (C, slot(2), U256::from(0x22))
            ]
        );
    }

    /// A line past [`STREAM_ABOVE`] is parsed as it is read; the result is
    /// the same as for a short one, and the next line is intact.
    #[test]
    fn a_long_line_streams_and_the_next_line_follows() {
        const N: u64 = 1000; // 1000 slots × 135 bytes ≈ 135 KB, past 64 KiB
        let mut dump = format!(r#"{{"address":"{A}","nonce":"0x1","balance":"0x0","storage":{{"#);
        for i in 0..N {
            if i > 0 {
                dump.push(',');
            }
            dump.push_str(&format!(r#""{}":"{}""#, slot(i + 1), slot(i + 1_000_001)));
        }
        dump.push_str("}}\n");
        dump.push_str(&format!(
            r#"{{"address":"{B}","nonce":"0x2","balance":"0x0"}}"#
        ));
        dump.push('\n');
        assert!(dump.len() > STREAM_ABOVE);

        let (accounts, slots) = drain(parse(Cursor::new(dump.clone()), etl()).unwrap());
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].0, A);
        assert_eq!(accounts[0].1.nonce, Some(1));
        assert_eq!(accounts[1].0, B);
        assert_eq!(slots.len(), N as usize);
        assert_eq!(slots[0], (A, slot(1), U256::from(1_000_001)));
        assert_eq!(
            slots[N as usize - 1],
            (A, slot(N), U256::from(1_000_000 + N))
        );

        // A long line at the very end, without a newline, parses too.
        dump.truncate(dump.rfind('\n').unwrap());
        let (accounts, _) = drain(parse(Cursor::new(dump), etl()).unwrap());
        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn a_line_without_an_address_is_an_error() {
        let dump = r#"{"nonce":"0x1","balance":"0x0"}"#.to_string() + "\n";
        let err = parse(Cursor::new(dump), etl()).unwrap_err();
        assert!(err.to_string().contains("address"), "{err}");
    }
}
