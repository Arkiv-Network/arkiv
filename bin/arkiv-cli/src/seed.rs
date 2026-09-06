//! `arkiv-cli seed-genesis`: build a pre-populated block-0 state.
//!
//! Wraps [`arkiv_seed`] for the command line: a spec from flags, progress on
//! stderr (and in a JSON file with `--progress-file`), and one of three outputs — a genesis JSON for `--chain <file>`, an
//! alloc-only JSON for ethereum-package's `additional_preloaded_contracts`, or
//! a `reth init-state` JSONL dump plus its `stateHash` genesis.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use arkiv_genesis::{Genesis, GenesisAccount};
use arkiv_seed::{AttributeTemplate, SeedManifest, SeedSpec, StreamSink, export};
use clap::{ArgAction, Args, ValueEnum};
use eyre::{Result, WrapErr, bail};

/// The chain id a seed derives keys under when nothing names one — reth's
/// `--dev` chain.
const DEFAULT_CHAIN_ID: u64 = 1337;

/// Above this many entities the genesis/alloc JSON is written compact; below,
/// pretty-printed for reading.
const PRETTY_PRINT_LIMIT: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SeedFormat {
    /// A full geth-format genesis with the seeded state in `alloc`, for
    /// `arkiv-reth node --chain <file>`.
    Genesis,
    /// The seeded accounts alone, as the object ethereum-package takes for
    /// `network_params.additional_preloaded_contracts`.
    Alloc,
    /// A `reth init-state` state dump, plus a genesis carrying `stateHash` and
    /// an empty `alloc` (see `--genesis-out`).
    Jsonl,
}

#[derive(Debug, Args)]
pub struct SeedGenesisArgs {
    /// Number of entities to seed.
    #[arg(long, default_value_t = 1000)]
    pub count: u64,

    /// Payload size in bytes of every entity.
    #[arg(long, default_value_t = 1024)]
    pub payload_size: usize,

    /// Content type of every entity.
    #[arg(long, default_value = "application/octet-stream")]
    pub content_type: String,

    /// An owner address; repeat for several (entities are dealt round-robin).
    /// Without any, the first `--dev-owners` mnemonic-derived dev accounts own
    /// the entities.
    #[arg(long = "owner")]
    pub owners: Vec<Address>,

    /// How many dev accounts own the entities when no `--owner` is given.
    #[arg(long, default_value_t = 1)]
    pub dev_owners: usize,

    /// Absolute expiry block of every entity, or `never`.
    #[arg(long, default_value = "never", value_parser = parse_expiry)]
    pub expires_at: u64,

    /// An attribute template `name:type=expr`; repeat for several. Types:
    /// u256, u64, i32, bool, str. Expressions: index, mod(n), const(v),
    /// cycle(a,b,...), prefix(p). Default: `rank:u256=mod(100)` and
    /// `team:str=cycle(red,green,blue)`; pass `none` for no attributes.
    #[arg(long = "attribute")]
    pub attributes: Vec<String>,

    /// Chain id the entity keys are derived under. Defaults to the base
    /// genesis's chainId, or 1337 without one.
    #[arg(long)]
    pub chain_id: Option<u64>,

    /// A geth-format genesis to build on: its config and alloc are kept and the
    /// seeded state merged in. Without one, a dev genesis (every fork at block
    /// 0, 1 gwei base fee) is used.
    #[arg(long)]
    pub genesis: Option<PathBuf>,

    /// Also fund the mnemonic-derived dev accounts, as `--dev` does.
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub fund_dev_accounts: bool,

    /// What to write.
    #[arg(long, value_enum, default_value_t = SeedFormat::Genesis)]
    pub format: SeedFormat,

    /// Output path.
    #[arg(long)]
    pub out: PathBuf,

    /// Where `--format jsonl` writes the `stateHash` genesis. Defaults to
    /// `<out>.genesis.json`.
    #[arg(long)]
    pub genesis_out: Option<PathBuf>,

    /// Where to write the manifest describing the seed (counts, owners, the
    /// state root, sample keys). Defaults to `<out>.manifest.json`.
    #[arg(long)]
    pub manifest_out: Option<PathBuf>,

    /// Seed of the payload byte generator.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,

    /// Entities per executor batch.
    #[arg(long, default_value_t = arkiv_seed::DEFAULT_BATCH_SIZE)]
    pub batch_size: usize,

    /// Keep a JSON progress file here, replaced about once a second, for a
    /// watcher: `phase` (seeding, finishing, done, failed), `percent`,
    /// entities done and total, `elapsed_s`, `updated_at` (unix seconds).
    #[arg(long)]
    pub progress_file: Option<PathBuf>,
}

/// How often `--progress-file` is rewritten while the seed builds.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// The `--progress-file` writer: the builder's per-batch progress, throttled,
/// as one JSON document replaced whole (written next to itself, then renamed).
struct ProgressFile {
    path: PathBuf,
    started: Instant,
    last_write: Option<Instant>,
    last: Option<arkiv_seed::Progress>,
    total: u64,
}

impl ProgressFile {
    fn new(path: &Path, total: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            started: Instant::now(),
            last_write: None,
            last: None,
            total,
        }
    }

    /// A batch finished; write if the last write is old enough.
    fn tick(&mut self, progress: arkiv_seed::Progress) {
        self.last = Some(progress);
        let due = self
            .last_write
            .is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
        if due {
            self.write("seeding", None, None);
        }
    }

    /// Every entity is built; the sink is finishing (leftover accounts, the
    /// sort merge, the root).
    fn finishing(&mut self) {
        self.write("finishing", None, None);
    }

    fn done(&mut self, root: B256) {
        self.write("done", Some(root), None);
    }

    fn failed(&mut self, error: &str) {
        self.write("failed", None, Some(error));
    }

    fn write(&mut self, phase: &str, root: Option<B256>, error: Option<&str>) {
        let done = self.last.as_ref().map_or(0, |p| p.done);
        let percent = match phase {
            "done" => Some(100.0),
            "failed" => None,
            _ if self.total == 0 => None,
            _ => Some((done as f64 * 1000.0 / self.total as f64).round() / 10.0),
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
            "entities_done": done,
            "entities_total": self.total,
            "cached_accounts": self.last.as_ref().map(|p| p.cached_accounts),
            "cached_slots": self.last.as_ref().map(|p| p.cached_slots),
            "state_root": root.map(|r| r.to_string()),
            "error": error,
        });
        let tmp = self.path.with_extension("json.tmp");
        let written = std::fs::write(&tmp, format!("{document:#}\n"))
            .and_then(|_| std::fs::rename(&tmp, &self.path));
        if let Err(e) = written {
            eprintln!("cannot write progress file {}: {e}", self.path.display());
        }
        self.last_write = Some(Instant::now());
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

pub fn run(args: SeedGenesisArgs) -> Result<()> {
    let base: Genesis = match &args.genesis {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .wrap_err_with(|| format!("read base genesis {}", path.display()))?;
            serde_json::from_str(&raw)
                .wrap_err_with(|| format!("parse base genesis {}", path.display()))?
        }
        None => export::dev_genesis(args.chain_id.unwrap_or(DEFAULT_CHAIN_ID)),
    };
    let chain_id = args.chain_id.unwrap_or(base.config.chain_id);
    if chain_id != base.config.chain_id {
        bail!(
            "--chain-id {chain_id} disagrees with the base genesis chainId {}",
            base.config.chain_id
        );
    }

    let owners = if args.owners.is_empty() {
        arkiv_genesis::dev_signers(args.dev_owners)?
            .iter()
            .map(|s| s.address())
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
            .map(|t| t.parse())
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
        batch_size: args.batch_size,
    };
    spec.validate()?;

    eprintln!(
        "seeding {} entities × {} B for {} owner(s) on chain {chain_id}",
        spec.count,
        spec.payload_size,
        spec.owners.len()
    );
    // Base accounts: the given genesis's alloc, plus the dev funding.
    let mut base_alloc: BTreeMap<Address, GenesisAccount> = base.alloc.clone();
    if args.fund_dev_accounts {
        for (address, account) in arkiv_genesis::genesis_alloc()? {
            base_alloc.entry(address).or_insert(account);
        }
    }

    let started = Instant::now();
    let progress_file = RefCell::new(
        args.progress_file
            .as_deref()
            .map(|path| ProgressFile::new(path, spec.count)),
    );
    let progress = |p: arkiv_seed::Progress| {
        eprintln!(
            "  {}/{} entities ({:.1} s; {} accounts, {} slots cached)",
            p.done,
            p.total,
            p.elapsed_ms as f64 / 1000.0,
            p.cached_accounts,
            p.cached_slots
        );
        if let Some(file) = progress_file.borrow_mut().as_mut() {
            file.tick(p);
            if p.done == p.total {
                file.finishing();
            }
        }
    };
    let pretty = spec.count <= PRETTY_PRINT_LIMIT;
    let built = build(&args, base, base_alloc, &spec, pretty, progress);
    let manifest = match built {
        Ok(manifest) => {
            if let Some(file) = progress_file.borrow_mut().as_mut() {
                file.done(manifest.state_root);
            }
            manifest
        }
        Err(e) => {
            if let Some(file) = progress_file.borrow_mut().as_mut() {
                file.failed(&format!("{e:#}"));
            }
            return Err(e);
        }
    };
    eprintln!(
        "built {} accounts, state root {} ({:.1} s)",
        manifest.accounts,
        manifest.state_root,
        started.elapsed().as_secs_f64()
    );
    eprintln!("wrote {:?} output to {}", args.format, args.out.display());

    let manifest_out = args
        .manifest_out
        .clone()
        .unwrap_or_else(|| sibling(&args.out, ".manifest.json"));
    write_json(&manifest_out, &manifest, true)?;
    eprintln!("wrote manifest to {}", manifest_out.display());
    Ok(())
}

/// Build the seed in the asked-for shape and write it.
fn build(
    args: &SeedGenesisArgs,
    base: Genesis,
    base_alloc: BTreeMap<Address, GenesisAccount>,
    spec: &SeedSpec,
    pretty: bool,
    progress: impl FnMut(arkiv_seed::Progress),
) -> Result<SeedManifest> {
    let manifest = match args.format {
        SeedFormat::Genesis => {
            let state = arkiv_seed::build_in_memory(spec, base_alloc, progress)?;
            let genesis = export::genesis_with_alloc(base, state.alloc);
            write_json(&args.out, &genesis, pretty)?;
            state.manifest
        }
        SeedFormat::Alloc => {
            let state = arkiv_seed::build_in_memory(spec, base_alloc, progress)?;
            write_json(&args.out, &state.alloc, pretty)?;
            state.manifest
        }
        SeedFormat::Jsonl => {
            // The dump streams out as the seed builds; the sort runs spill
            // next to it, and are gone once the root is in its first line.
            let mut sink = StreamSink::create(&args.out, sibling(&args.out, ".sort"))?;
            let manifest = arkiv_seed::build(spec, base_alloc, &mut sink, progress)?;
            let genesis_out = args
                .genesis_out
                .clone()
                .unwrap_or_else(|| sibling(&args.out, ".genesis.json"));
            let genesis = export::genesis_with_state_hash(base, manifest.state_root)?;
            write_json(&genesis_out, &genesis, true)?;
            eprintln!("wrote stateHash genesis to {}", genesis_out.display());
            manifest
        }
    };
    Ok(manifest)
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T, pretty: bool) -> Result<()> {
    let file = File::create(path).wrap_err_with(|| format!("create {}", path.display()))?;
    let mut out = BufWriter::new(file);
    if pretty {
        serde_json::to_writer_pretty(&mut out, value)?;
    } else {
        serde_json::to_writer(&mut out, value)?;
    }
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
