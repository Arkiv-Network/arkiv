//! `arkiv-cli seed-genesis`: prefill a GolemDB chain's genesis.
//!
//! Writes the seeded entities and the funded accounts into the Arkiv Store as
//! its genesis commit, seals it, names the store's root as the genesis
//! `stateHash`, and commits it tagged with the resulting genesis hash. What it
//! leaves behind is a store a node adopts at start-up and the genesis file to
//! start that node with (`arkiv-reth node --chain <out>`, with the same
//! `ARKIV_STORE_URL` and `ARKIV_STORE_NAMESPACE`).
//!
//! The genesis is written with an **empty alloc** by default: the funded
//! accounts are inside the store's genesis commit, so inside `stateHash`, and
//! "`stateHash`, empty alloc" is the shape the consensus-genesis tooling (with
//! arkiv-prefill's stateHash patch) and db-chain's registry already expect of
//! a seeded genesis. `--keep-alloc` writes them out as well.
//!
//! A **replay** writes the same seed into another (empty) store -- a second
//! node's -- from the same base genesis, and refuses to commit unless it seals
//! to the expected root: `--expect-state-root`, or the base genesis's own
//! `stateHash` when it names one.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use arkiv_reth_statemanager::GenesisAccount;
use arkiv_seed::{
    AttributeTemplate, GenesisWriter, Input, SeedEntity, SeedManifest, SeedSpec, genesis,
};
use clap::{ArgAction, Args};
use eyre::{Result, WrapErr, bail};
use serde_json::Value;

/// The chain id a synthetic seed is built for when nothing names one --
/// reth's `--dev` chain.
const DEFAULT_CHAIN_ID: u64 = 1337;

/// How often `--progress-file` is rewritten, and stderr reports.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Args)]
pub struct SeedGenesisArgs {
    /// An explicit-key seed index (`seed.json` and its JSON Lines parts), as
    /// the Sourcify export writes it. Without one, a synthetic seed is built
    /// from the flags below.
    #[arg(long)]
    pub input: Option<PathBuf>,

    /// Synthetic: number of entities.
    #[arg(long, default_value_t = 1000)]
    pub count: u64,

    /// Synthetic: payload size in bytes of every entity.
    #[arg(long, default_value_t = 1024)]
    pub payload_size: usize,

    /// Synthetic: content type of every entity.
    #[arg(long, default_value = "application/octet-stream")]
    pub content_type: String,

    /// Synthetic: an owner address; repeat for several (entities are dealt
    /// round-robin). Without any, the first `--dev-owners` mnemonic-derived
    /// dev accounts own the entities.
    #[arg(long = "owner")]
    pub owners: Vec<Address>,

    /// Synthetic: how many dev accounts own the entities when no `--owner` is
    /// given.
    #[arg(long, default_value_t = 1)]
    pub dev_owners: usize,

    /// Synthetic: absolute expiry block of every entity, or `never`.
    #[arg(long, default_value = "never", value_parser = parse_expiry)]
    pub expires_at: u64,

    /// Synthetic: an attribute template `name:type=expr`; repeat for several.
    /// Types: u256, u64, i32, bool, str. Expressions: index, mod(n), const(v),
    /// cycle(a,b,...), prefix(p). Default: `rank:u256=mod(100)` and
    /// `team:str=cycle(red,green,blue)`; pass `none` for no attributes.
    #[arg(long = "attribute")]
    pub attributes: Vec<String>,

    /// Synthetic: seed of the payload byte generator.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,

    /// Chain id of the genesis. Defaults to the base genesis's, else the
    /// input's, else 1337.
    #[arg(long)]
    pub chain_id: Option<u64>,

    /// A geth-format genesis to build on: its config and alloc are kept and
    /// `stateHash` set. Without one, a dev genesis (every fork at block 0,
    /// 1 gwei base fee) is used. If it already names a `stateHash`, the seed
    /// must seal to exactly that root (a replay into another node's store).
    #[arg(long)]
    pub genesis: Option<PathBuf>,

    /// Also fund the mnemonic-derived dev accounts, as `--dev` does.
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub fund_dev_accounts: bool,

    /// Keep the funded accounts in the written genesis's `alloc` as well. By
    /// default the alloc is emptied: the accounts are already inside the
    /// store's genesis commit, and so inside `stateHash`.
    #[arg(long)]
    pub keep_alloc: bool,

    /// The root the seed must seal to, or nothing is committed: a replay, into
    /// another node's store, of a genesis written earlier (pass that run's base
    /// genesis as `--genesis`). Defaults to the base genesis's own `stateHash`.
    #[arg(long)]
    pub expect_state_root: Option<B256>,

    /// The Arkiv store the genesis commit is written into: a Valkey URL, the
    /// node's `ARKIV_STORE_URL`.
    #[arg(long, env = "ARKIV_STORE_URL")]
    pub store_url: String,

    /// The store namespace, the node's `ARKIV_STORE_NAMESPACE`. It must be
    /// empty.
    #[arg(long, env = "ARKIV_STORE_NAMESPACE", default_value = "arkiv")]
    pub store_namespace: String,

    /// Where to write the genesis JSON, `stateHash` included.
    #[arg(long)]
    pub out: PathBuf,

    /// Where to write the manifest (counts, owners, root, sample keys).
    /// Defaults to `<out>.manifest.json`.
    #[arg(long)]
    pub manifest_out: Option<PathBuf>,

    /// Keep a JSON progress file here, replaced about once a second (also
    /// while sealing and committing, when no entity moves), for a watcher:
    /// `phase` (seeding, finishing, done, failed), `percent`, entities done
    /// and total, `elapsed_s`, `updated_at` (unix seconds), `state_root`.
    #[arg(long)]
    pub progress_file: Option<PathBuf>,
}

/// Where the entities come from.
enum Source {
    Spec(SeedSpec),
    Input(Box<Input>),
}

pub fn run(args: SeedGenesisArgs) -> Result<()> {
    let mut progress = args
        .progress_file
        .as_deref()
        .map(|path| ProgressFile::new(path, &args.store_namespace));
    let outcome = seed(&args, progress.as_mut());
    if let Err(error) = &outcome
        && let Some(progress) = progress.as_mut()
    {
        progress.failed(&format!("{error:#}"));
    }
    outcome
}

fn seed(args: &SeedGenesisArgs, mut progress: Option<&mut ProgressFile>) -> Result<()> {
    let input = args
        .input
        .as_deref()
        .map(|path| Input::open(path).map(|input| (path.to_path_buf(), input)))
        .transpose()?;

    let mut genesis = match &args.genesis {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .wrap_err_with(|| format!("read base genesis {}", path.display()))?;
            serde_json::from_str::<Value>(&raw)
                .wrap_err_with(|| format!("parse base genesis {}", path.display()))?
        }
        None => genesis::dev_genesis(
            args.chain_id
                .or(input.as_ref().map(|(_, input)| input.chain_id))
                .unwrap_or(DEFAULT_CHAIN_ID),
        ),
    };
    let chain_id = genesis::chain_id(&genesis)?;
    if let Some(wanted) = args.chain_id
        && wanted != chain_id
    {
        bail!("--chain-id {wanted} disagrees with the base genesis chainId {chain_id}");
    }
    let expected_root = match (args.expect_state_root, genesis::state_hash(&genesis)?) {
        (Some(flag), Some(named)) if flag != named => {
            bail!("--expect-state-root {flag} disagrees with the base genesis's stateHash {named}")
        }
        (flag, named) => flag.or(named),
    };
    if args.fund_dev_accounts {
        let dev = arkiv_genesis::genesis_alloc()?
            .into_iter()
            .map(|(address, account)| (address, account.balance));
        genesis::fund(&mut genesis, dev);
    }

    let (source, total) = match input {
        Some((path, input)) => {
            if input.chain_id != chain_id {
                // Explicit keys do not depend on the chain id, so this is the
                // chain the export came from rather than an error.
                eprintln!(
                    "note: the input was exported for chain {}, the genesis is chain {chain_id}",
                    input.chain_id
                );
            }
            eprintln!("counting the input's entities ...");
            let total = arkiv_seed::count_entities(&path)?;
            (Source::Input(Box::new(input)), total)
        }
        None => {
            let spec = synthetic_spec(args, chain_id)?;
            let total = spec.count;
            (Source::Spec(spec), total)
        }
    };

    let alloc: Vec<GenesisAccount> = genesis::alloc(&genesis)?
        .into_iter()
        .map(|(address, funded)| GenesisAccount {
            address,
            balance: funded.balance,
            nonce: funded.nonce,
        })
        .collect();

    let with_code = genesis::accounts_with_code(&genesis);
    if with_code > 0 {
        eprintln!(
            "note: {with_code} alloc accounts carry code or storage; a GolemDB genesis keeps \
             their balance and nonce only"
        );
    }
    eprintln!(
        "seeding {total} entities and {} funded accounts into {} (namespace {}) for chain {chain_id}",
        alloc.len(),
        args.store_url,
        args.store_namespace
    );
    let store = arkiv_valkey::ValkeyStore::connect(&args.store_url, &args.store_namespace)
        .map_err(|e| eyre::eyre!("connect to the Arkiv store at {}: {e:?}", args.store_url))?;

    let started = Instant::now();
    if let Some(progress) = progress.as_deref_mut() {
        progress.start(total);
    }
    let mut writer = GenesisWriter::begin(&store, alloc)?;
    let mut report = Report::new(total, started);
    let mut add = |writer: &mut GenesisWriter<'_, _>, entity: SeedEntity| -> Result<()> {
        writer.add(entity)?;
        if report.due() {
            report.print(writer.entities());
            if let Some(progress) = progress.as_deref_mut() {
                progress.tick(writer.entities());
            }
        }
        Ok(())
    };
    let mut source = source;
    match &mut source {
        Source::Spec(spec) => {
            for i in 0..spec.count {
                add(&mut writer, spec.entity(i)?)?;
            }
        }
        Source::Input(input) => {
            while let Some(entity) = input.next_entity()? {
                add(&mut writer, entity)?;
            }
        }
    }
    report.print(writer.entities());
    if let Some(progress) = progress.as_deref_mut() {
        progress.tick(writer.entities());
        progress.finishing();
    }

    eprintln!("sealing the genesis commit ...");
    let sealed = writer.seal()?;
    let summary = sealed.summary.clone();
    if let Some(expected) = expected_root
        && expected != summary.state_root
    {
        sealed.discard()?;
        bail!(
            "the seed seals to {}, but {expected} was expected: this is not the seed (or not \
             the base genesis) that root was written for",
            summary.state_root
        );
    }
    genesis::set_state_hash(&mut genesis, summary.state_root);
    if !args.keep_alloc {
        genesis::clear_alloc(&mut genesis);
    }
    let genesis_hash = genesis::genesis_hash(&genesis)?;
    eprintln!(
        "state root {}, genesis hash {genesis_hash}; committing ...",
        summary.state_root
    );
    sealed.commit(genesis_hash)?;

    write_json(&args.out, &genesis)?;
    eprintln!("wrote the genesis to {}", args.out.display());

    let manifest = manifest(
        &source,
        chain_id,
        &summary,
        genesis_hash,
        &args.store_namespace,
    );
    let manifest_out = args
        .manifest_out
        .clone()
        .unwrap_or_else(|| sibling(&args.out, ".manifest.json"));
    write_json(&manifest_out, &manifest)?;
    eprintln!("wrote the manifest to {}", manifest_out.display());

    if let Some(progress) = progress {
        progress.done(summary.state_root);
    }
    eprintln!(
        "seeded {} entities in {:.1} s",
        summary.entities,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn synthetic_spec(args: &SeedGenesisArgs, chain_id: u64) -> Result<SeedSpec> {
    let owners = if args.owners.is_empty() {
        arkiv_genesis::dev_signers(args.dev_owners)?
            .iter()
            .map(|signer| signer.address())
            .collect()
    } else {
        args.owners.clone()
    };
    let attributes: Vec<AttributeTemplate> = if args.attributes.is_empty() {
        SeedSpec::default_attributes()
    } else if args.attributes.len() == 1 && args.attributes[0].eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        args.attributes
            .iter()
            .map(|template| template.parse())
            .collect::<Result<_>>()?
    };
    let spec = SeedSpec {
        chain_id,
        count: args.count,
        payload_size: args.payload_size,
        content_type: args.content_type.clone(),
        owners,
        expires_at: args.expires_at,
        attributes,
        seed: args.seed,
    };
    spec.validate()?;
    Ok(spec)
}

fn manifest(
    source: &Source,
    chain_id: u64,
    summary: &arkiv_seed::Summary,
    genesis_hash: B256,
    namespace: &str,
) -> SeedManifest {
    let accounts = summary.records - summary.entities;
    match source {
        Source::Spec(spec) => SeedManifest {
            chain_id,
            count: summary.entities,
            payload_size: spec.payload_size,
            content_type: spec.content_type.clone(),
            owners: spec.owners.clone(),
            expires_at: spec.expires_at,
            attributes: spec.attributes.iter().map(ToString::to_string).collect(),
            seed: spec.seed,
            state_root: summary.state_root,
            accounts,
            sample_keys: summary.sample_keys.clone(),
            owner_nonces: summary.minted.clone(),
            genesis_hash,
            store_namespace: namespace.to_owned(),
        },
        Source::Input(_) => SeedManifest {
            chain_id,
            count: summary.entities,
            payload_size: 0,
            content_type: String::new(),
            owners: summary.creators.clone(),
            expires_at: u64::MAX,
            attributes: Vec::new(),
            seed: 0,
            state_root: summary.state_root,
            accounts,
            sample_keys: summary.sample_keys.clone(),
            // Explicit keys mint nothing.
            owner_nonces: summary
                .creators
                .iter()
                .map(|creator| (*creator, 0))
                .collect(),
            genesis_hash,
            store_namespace: namespace.to_owned(),
        },
    }
}

/// Throttled progress on stderr.
struct Report {
    total: u64,
    started: Instant,
    last: Instant,
}

impl Report {
    fn new(total: u64, started: Instant) -> Self {
        Self {
            total,
            started,
            last: started,
        }
    }

    fn due(&mut self) -> bool {
        if self.last.elapsed() < PROGRESS_EVERY {
            return false;
        }
        self.last = Instant::now();
        true
    }

    fn print(&self, done: u64) {
        eprintln!(
            "  {done}/{} entities ({:.1} s)",
            self.total,
            self.started.elapsed().as_secs_f64()
        );
    }
}

/// The `--progress-file` writer: one JSON document replaced whole (written
/// next to itself, then renamed).
///
/// A watcher takes a file that stops moving for a process that is gone, so
/// the finishing phase -- seal and commit, minutes on a large seed, with no
/// entity to report -- keeps a heartbeat thread rewriting the same document
/// until `done` or `failed`.
struct ProgressFile {
    document: ProgressDocument,
    last_write: Option<Instant>,
    heartbeat: Option<Heartbeat>,
}

#[derive(Clone)]
struct ProgressDocument {
    path: PathBuf,
    namespace: String,
    started: Instant,
    done: u64,
    total: u64,
}

struct Heartbeat {
    /// Dropping it wakes the thread, which then exits.
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

impl ProgressFile {
    fn new(path: &Path, namespace: &str) -> Self {
        Self {
            document: ProgressDocument {
                path: path.to_path_buf(),
                namespace: namespace.to_owned(),
                started: Instant::now(),
                done: 0,
                total: 0,
            },
            last_write: None,
            heartbeat: None,
        }
    }

    fn start(&mut self, total: u64) {
        self.document.total = total;
        self.document.started = Instant::now();
        self.write("seeding", None, None);
    }

    fn tick(&mut self, done: u64) {
        self.document.done = done;
        if self
            .last_write
            .is_none_or(|at| at.elapsed() >= PROGRESS_EVERY)
        {
            self.write("seeding", None, None);
        }
    }

    fn finishing(&mut self) {
        self.write("finishing", None, None);
        if self.heartbeat.is_some() {
            return;
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let document = self.document.clone();
        let thread = std::thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(PROGRESS_EVERY) {
                document.write("finishing", None, None);
            }
        });
        self.heartbeat = Some(Heartbeat { stop, thread });
    }

    fn done(&mut self, root: B256) {
        self.stop_heartbeat();
        self.write("done", Some(root), None);
    }

    fn failed(&mut self, error: &str) {
        self.stop_heartbeat();
        self.write("failed", None, Some(error));
    }

    fn stop_heartbeat(&mut self) {
        if let Some(Heartbeat { stop, thread }) = self.heartbeat.take() {
            drop(stop);
            let _ = thread.join();
        }
    }

    fn write(&mut self, phase: &str, root: Option<B256>, error: Option<&str>) {
        self.document.write(phase, root, error);
        self.last_write = Some(Instant::now());
    }
}

impl ProgressDocument {
    fn write(&self, phase: &str, root: Option<B256>, error: Option<&str>) {
        let percent = match phase {
            "done" => Some(100.0),
            "failed" => None,
            _ if self.total == 0 => None,
            _ => Some((self.done as f64 * 1000.0 / self.total as f64).round() / 10.0),
        };
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let document = serde_json::json!({
            "pid": std::process::id(),
            "phase": phase,
            "percent": percent,
            "elapsed_s": (self.started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "updated_at": updated_at,
            "entities_done": self.done,
            "entities_total": self.total,
            "store_namespace": self.namespace,
            "state_root": root.map(|r| r.to_string()),
            "error": error,
        });
        let tmp = self.path.with_extension("json.tmp");
        let written = std::fs::write(&tmp, format!("{document:#}\n"))
            .and_then(|_| std::fs::rename(&tmp, &self.path));
        if let Err(e) = written {
            eprintln!("cannot write progress file {}: {e}", self.path.display());
        }
    }
}

fn parse_expiry(s: &str) -> std::result::Result<u64, String> {
    if s.eq_ignore_ascii_case("never") || s.eq_ignore_ascii_case("max") {
        return Ok(u64::MAX);
    }
    s.parse::<u64>()
        .map_err(|e| format!("expected a block number or `never`: {e}"))
}

fn sibling(out: &Path, suffix: &str) -> PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    out.with_file_name(name)
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = File::create(path).wrap_err_with(|| format!("create {}", path.display()))?;
    let mut out = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
