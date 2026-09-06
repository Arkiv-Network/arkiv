//! A progress file for `init-state`, rewritten every second, so a watcher can
//! tell a slow import from a dead one.
//!
//! reth's importer reports through its log alone: parsed accounts every 100k,
//! written accounts every 100k and at every commit, and the position of the
//! state-root walk at every flush. [`ProgressLayer`] is a tracing layer that
//! picks those events up; [`CountingReader`] adds how much of the dump has
//! been read, which is the only signal during the parse; and
//! [`ProgressWriter`] dumps the whole as JSON on a timer. The file is
//! replaced atomically (written next to itself, then renamed), so a reader
//! never sees half a document.
//!
//! The document, with every number a plain JSON number:
//!
//! ```json
//! {
//!   "pid": 4242,
//!   "phase": "writing",
//!   "percent": 37.5,
//!   "elapsed_s": 812.4,
//!   "phase_elapsed_s": 301.0,
//!   "updated_at": 1757168522.4,
//!   "dump": { "path": "state.jsonl", "bytes_read": 1, "bytes_total": 2, "accounts_parsed": 3 },
//!   "write": { "accounts_written": 4, "accounts_committed": 5, "accounts_total": 6 },
//!   "hash": { "position": "0x9c…", "trie_updates": 7 },
//!   "state_root": null,
//!   "block_hash": null,
//!   "error": null
//! }
//! ```
//!
//! `phase` runs `starting`, `parsing`, `writing`, `hashing`, then `done` or
//! `failed`. `percent` is of the current phase (bytes of the dump read;
//! accounts written of the total; the hashed-key position of the root walk,
//! which is uniform over the key space) and `null` when the phase has no
//! measure yet. `updated_at` moves every write even when nothing else does:
//! a file older than a few seconds means the process is gone.

use alloy_primitives::B256;
use reth_tracing::tracing_subscriber::Layer;
use reth_tracing::tracing_subscriber::layer::Context;
use reth_tracing::tracing_subscriber::registry::Registry;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::field::{Field, Visit};
use tracing::{Event, Level};

/// The log target reth's importer reports under.
const IMPORTER_TARGET: &str = "reth::cli";

/// How many bytes the reader counts up locally before touching the shared
/// state: serde reads the dump a byte at a time.
const READ_PUBLISH_EVERY: u64 = 1 << 20;

/// Where the import is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Opening the datadir, before the first byte of the dump is read.
    Starting,
    /// Parsing the dump into reth's sorted collector.
    Parsing,
    /// Writing accounts and storage to the database.
    Writing,
    /// Walking the hashed tables to rebuild the trie and the state root.
    Hashing,
    /// The import returned; the root matched.
    Done,
    /// The import returned an error.
    Failed,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Parsing => "parsing",
            Self::Writing => "writing",
            Self::Hashing => "hashing",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
struct State {
    phase: Phase,
    started: Instant,
    phase_started: Instant,
    dump_path: PathBuf,
    bytes_total: u64,
    bytes_read: u64,
    accounts_parsed: u64,
    accounts_written: u64,
    accounts_committed: u64,
    accounts_total: Option<u64>,
    hash_position: Option<B256>,
    trie_updates: u64,
    state_root: Option<B256>,
    block_hash: Option<B256>,
    error: Option<String>,
}

impl State {
    /// Move to `phase` if it is later than the current one; phases never go
    /// back, so a late log line cannot undo a transition.
    fn advance(&mut self, phase: Phase) {
        if phase > self.phase {
            self.phase = phase;
            self.phase_started = Instant::now();
        }
    }

    /// Progress through the current phase, in percent.
    fn percent(&self) -> Option<f64> {
        let fraction = match self.phase {
            Phase::Starting => return None,
            Phase::Parsing => {
                if self.bytes_total == 0 {
                    return None;
                }
                self.bytes_read as f64 / self.bytes_total as f64
            }
            Phase::Writing => {
                let total = self.accounts_total?;
                if total == 0 {
                    return None;
                }
                self.accounts_written as f64 / total as f64
            }
            Phase::Hashing => key_fraction(self.hash_position?),
            Phase::Done => 1.0,
            Phase::Failed => return None,
        };
        Some((fraction * 1000.0).round() / 10.0)
    }

    fn document(&self) -> serde_json::Value {
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        serde_json::json!({
            "pid": std::process::id(),
            "phase": self.phase.name(),
            "percent": self.percent(),
            "elapsed_s": tenths(self.started.elapsed()),
            "phase_elapsed_s": tenths(self.phase_started.elapsed()),
            "updated_at": updated_at,
            "dump": {
                "path": self.dump_path.display().to_string(),
                "bytes_read": self.bytes_read,
                "bytes_total": self.bytes_total,
                "accounts_parsed": self.accounts_parsed,
            },
            "write": {
                "accounts_written": self.accounts_written,
                "accounts_committed": self.accounts_committed,
                "accounts_total": self.accounts_total,
            },
            "hash": {
                "position": self.hash_position.map(|k| k.to_string()),
                "trie_updates": self.trie_updates,
            },
            "state_root": self.state_root.map(|r| r.to_string()),
            "block_hash": self.block_hash.map(|h| h.to_string()),
            "error": self.error,
        })
    }
}

/// Where a hashed key sits in the key space, as a fraction of it. Hashed keys
/// are uniform, so this is the fraction of the accounts behind the walk.
fn key_fraction(key: B256) -> f64 {
    let head = u64::from_be_bytes(key[..8].try_into().expect("8 bytes"));
    head as f64 / (u64::MAX as f64 + 1.0)
}

fn tenths(d: Duration) -> f64 {
    (d.as_secs_f64() * 10.0).round() / 10.0
}

/// The shared progress of one import.
#[derive(Debug, Clone)]
pub struct Progress(Arc<Mutex<State>>);

impl Progress {
    /// Progress for importing the dump at `dump_path`, whose size is read
    /// now for the parse phase's denominator.
    pub fn new(dump_path: &Path) -> io::Result<Self> {
        let bytes_total = std::fs::metadata(dump_path)?.len();
        let now = Instant::now();
        Ok(Self(Arc::new(Mutex::new(State {
            phase: Phase::Starting,
            started: now,
            phase_started: now,
            dump_path: dump_path.to_path_buf(),
            bytes_total,
            bytes_read: 0,
            accounts_parsed: 0,
            accounts_written: 0,
            accounts_committed: 0,
            accounts_total: None,
            hash_position: None,
            trie_updates: 0,
            state_root: None,
            block_hash: None,
            error: None,
        }))))
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        // A panic while holding the lock leaves the state readable; nothing
        // here can leave it inconsistent.
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state);
    }

    /// The dump reader, counting what it hands out.
    pub fn reader<R: BufRead>(&self, inner: R) -> CountingReader<R> {
        CountingReader {
            inner,
            progress: self.clone(),
            pending: 0,
            started: false,
        }
    }

    /// The tracing layer that follows reth's importer log.
    pub fn layer(&self) -> ProgressLayer {
        ProgressLayer(self.clone())
    }

    /// The import returned successfully with the block hash it wrote.
    pub fn finish(&self, block_hash: B256) {
        self.update(|s| {
            s.block_hash = Some(block_hash);
            s.advance(Phase::Done);
        });
    }

    /// The import returned an error.
    pub fn fail(&self, error: impl fmt::Display) {
        self.update(|s| {
            s.error = Some(error.to_string());
            s.advance(Phase::Failed);
        });
    }

    /// The current document.
    pub fn document(&self) -> serde_json::Value {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).document()
    }

    /// Write the document to `path` every `every`, and once more when the
    /// writer is dropped. A failed write is logged once and never fails the
    /// import.
    pub fn spawn_writer(&self, path: PathBuf, every: Duration) -> ProgressWriter {
        let progress = self.clone();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("init-state-progress".into())
            .spawn(move || {
                let mut warned = false;
                loop {
                    let done = matches!(
                        stopped.recv_timeout(every),
                        Ok(()) | Err(RecvTimeoutError::Disconnected)
                    );
                    if let Err(e) = write_atomically(&path, &progress.document())
                        && !warned
                    {
                        tracing::warn!(
                            target: "arkiv-reth",
                            path = %path.display(),
                            %e,
                            "cannot write the init-state progress file"
                        );
                        warned = true;
                    }
                    if done {
                        return;
                    }
                }
            })
            .expect("spawn the progress writer thread");
        ProgressWriter {
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn read(&self, n: u64) {
        self.update(|s| {
            s.bytes_read = (s.bytes_read + n).min(s.bytes_total.max(s.bytes_read + n));
            s.advance(Phase::Parsing);
        });
    }

    fn read_to_end(&self) {
        self.update(|s| s.advance(Phase::Writing));
    }
}

/// Writes `document` to `path` by way of a sibling temporary file and a
/// rename, so a reader sees the old document or the new one, never a torn one.
fn write_atomically(path: &Path, document: &serde_json::Value) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut file, document)?;
    file.write_all(b"\n")?;
    file.flush()?;
    std::fs::rename(&tmp, path)
}

/// The writer thread; stops and writes the final document when dropped.
#[derive(Debug)]
pub struct ProgressWriter {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ProgressWriter {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A `BufRead` that reports what has been read from it. serde reads the dump
/// a byte at a time, so the count is published in [`READ_PUBLISH_EVERY`]
/// steps, and in full at end of file.
#[derive(Debug)]
pub struct CountingReader<R> {
    inner: R,
    progress: Progress,
    pending: u64,
    started: bool,
}

impl<R> CountingReader<R> {
    fn count(&mut self, n: usize) {
        if !self.started {
            // The first byte starts the parse, before any count is published.
            self.started = true;
            self.progress.read(0);
        }
        if n == 0 {
            // End of file, or an empty read: publish what is pending. Only a
            // real end of file follows the dump's last byte, and phases never
            // go back, so a stray empty read cannot mislead.
            self.progress.read(std::mem::take(&mut self.pending));
            self.progress.read_to_end();
            return;
        }
        self.pending += n as u64;
        if self.pending >= READ_PUBLISH_EVERY {
            self.progress.read(std::mem::take(&mut self.pending));
        }
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count(n);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for CountingReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        // An empty buffer is end of file, which `consume` never sees.
        if self.inner.fill_buf()?.is_empty() {
            self.count(0);
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
        if amt > 0 {
            self.count(amt);
        }
    }
}

/// A tracing layer following the importer's `reth::cli` events.
#[derive(Debug)]
pub struct ProgressLayer(Progress);

impl Layer<Registry> for ProgressLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, Registry>) {
        // The importer reports at INFO. Below that, opening the datadir
        // computes the empty genesis root with trace events carrying the
        // same field names.
        let metadata = event.metadata();
        if metadata.target() != IMPORTER_TARGET
            || !matches!(*metadata.level(), Level::INFO | Level::WARN | Level::ERROR)
        {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.update(|s| fields.apply(s));
    }
}

/// The fields of one importer event that matter here.
#[derive(Debug, Default)]
struct Fields {
    message: String,
    parsed_accounts: Option<u64>,
    total_accounts: Option<u64>,
    accounts_len: Option<u64>,
    last_account_key: Option<B256>,
    total_flushed_updates: Option<u64>,
    root: Option<B256>,
}

impl Fields {
    fn apply(&self, s: &mut State) {
        if let Some(n) = self.parsed_accounts {
            s.accounts_parsed = n;
            s.advance(Phase::Parsing);
        }
        if let Some(n) = self.total_accounts {
            s.accounts_written = n;
            s.advance(Phase::Writing);
            if self.message.starts_with("Committed") {
                s.accounts_committed = n;
            }
            if self.message.starts_with("All accounts written") {
                // The final count: a small dump never logs a running total.
                s.accounts_committed = n;
                s.accounts_total = Some(n);
            }
        }
        if let Some(n) = self.accounts_len {
            s.accounts_total = Some(n);
        }
        if let Some(key) = self.last_account_key {
            s.hash_position = Some(key);
            s.advance(Phase::Hashing);
        }
        if let Some(n) = self.total_flushed_updates {
            s.trie_updates = n;
            s.advance(Phase::Hashing);
        }
        if let Some(root) = self.root
            && self.message.starts_with("State root computation complete")
        {
            s.state_root = Some(root);
            s.hash_position = Some(B256::repeat_byte(0xff));
        }
        if self
            .message
            .starts_with("All accounts written to database, starting state root")
        {
            s.advance(Phase::Hashing);
        }
    }

    fn set_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "parsed_accounts" => self.parsed_accounts = Some(value),
            "total_accounts" => self.total_accounts = Some(value),
            "accounts_len" => self.accounts_len = Some(value),
            "total_flushed_updates" => self.total_flushed_updates = Some(value),
            _ => {}
        }
    }

    fn set_text(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_owned(),
            "last_account_key" => self.last_account_key = value.parse().ok(),
            "root" => self.root = value.parse().ok(),
            _ => {}
        }
    }
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set_u64(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if let Ok(value) = u64::try_from(value) {
            self.set_u64(field, value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.set_text(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%value` and `?value` both land here; a hash prints as `0x…` either
        // way, and the message as itself.
        self.set_text(field, &format!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_tracing::tracing_subscriber::layer::SubscriberExt;
    use std::io::BufReader;

    fn progress_over(bytes: &[u8]) -> (Progress, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        (Progress::new(file.path()).unwrap(), file)
    }

    fn with_layer(progress: &Progress, f: impl FnOnce()) {
        let subscriber = Registry::default().with(progress.layer());
        tracing::subscriber::with_default(subscriber, f);
    }

    #[test]
    fn the_reader_measures_the_parse() {
        let bytes = vec![b'x'; (READ_PUBLISH_EVERY + 10) as usize];
        let (progress, _file) = progress_over(&bytes);
        assert_eq!(progress.document()["phase"], "starting");
        assert!(progress.document()["percent"].is_null());

        let mut reader = progress.reader(BufReader::new(bytes.as_slice()));
        let mut buf = [0u8; 4096];
        reader.read_exact(&mut buf).unwrap();
        let doc = progress.document();
        assert_eq!(doc["phase"], "parsing");
        assert_eq!(doc["dump"]["bytes_read"], 0, "published in steps");

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        let doc = progress.document();
        assert_eq!(doc["dump"]["bytes_read"], bytes.len() as u64);
        assert_eq!(doc["dump"]["bytes_total"], bytes.len() as u64);
        assert_eq!(doc["phase"], "writing", "end of file ends the parse");
    }

    #[test]
    fn a_buf_read_consumer_is_counted_too() {
        let (progress, _file) = progress_over(b"first line\nsecond\n");
        let mut reader = progress.reader(BufReader::new(&b"first line\nsecond\n"[..]));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "first line\n");
        reader.read_line(&mut line).unwrap();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        assert_eq!(progress.document()["dump"]["bytes_read"], 18);
    }

    #[test]
    fn importer_events_drive_the_phases() {
        let (progress, _file) = progress_over(b"{}");
        with_layer(&progress, || {
            tracing::info!(target: "reth::cli", parsed_accounts = 200_000usize, "Parsed accounts");
        });
        let doc = progress.document();
        assert_eq!(doc["phase"], "parsing");
        assert_eq!(doc["dump"]["accounts_parsed"], 200_000);

        with_layer(&progress, || {
            tracing::info!(target: "reth::cli", total_accounts = 300_000usize, accounts_len = 1_200_000usize, "Writing accounts...");
            tracing::info!(target: "reth::cli", total_accounts = 310_000usize, accounts_len = 1_200_000usize, storage_units = 7usize, "Committed chunk");
            // Another target's fields are not the importer's, nor are the
            // genesis root's trace events.
            tracing::info!(target: "other", total_accounts = 5usize, "noise");
            tracing::trace!(target: "reth::cli", total_flushed_updates = 9usize, "State root has been computed");
        });
        let doc = progress.document();
        assert_eq!(doc["phase"], "writing");
        assert_eq!(doc["write"]["accounts_written"], 310_000);
        assert_eq!(doc["write"]["accounts_committed"], 310_000);
        assert_eq!(doc["write"]["accounts_total"], 1_200_000);
        assert_eq!(doc["percent"], 25.8);

        with_layer(&progress, || {
            tracing::info!(target: "reth::cli", total_accounts = 1_200_000usize, "All accounts written to database");
        });
        let doc = progress.document();
        assert_eq!(doc["write"]["accounts_committed"], 1_200_000);
        assert_eq!(doc["percent"], 100.0);

        let key = B256::repeat_byte(0x40);
        with_layer(&progress, || {
            tracing::info!(target: "reth::cli", last_account_key = %key, updated_len = 25_000usize, total_flushed_updates = 50_000usize, "Flushing trie updates (committing to free memory)");
        });
        let doc = progress.document();
        assert_eq!(doc["phase"], "hashing");
        assert_eq!(doc["hash"]["position"], key.to_string());
        assert_eq!(doc["hash"]["trie_updates"], 50_000);
        assert_eq!(doc["percent"], 25.1);

        let root = B256::repeat_byte(0xab);
        with_layer(&progress, || {
            tracing::info!(target: "reth::cli", %root, updated_len = 1usize, total_flushed_updates = 50_001usize, "State root computation complete");
        });
        let doc = progress.document();
        assert_eq!(doc["state_root"], root.to_string());
        assert_eq!(doc["percent"], 100.0);

        progress.finish(B256::repeat_byte(0x01));
        let doc = progress.document();
        assert_eq!(doc["phase"], "done");
        assert_eq!(doc["block_hash"], B256::repeat_byte(0x01).to_string());
        assert!(doc["error"].is_null());
    }

    #[test]
    fn a_failure_is_recorded_and_phases_never_go_back() {
        let (progress, _file) = progress_over(b"{}");
        progress.fail("boom");
        with_layer(&progress, || {
            tracing::info!(target: "reth::cli", parsed_accounts = 1usize, "Parsed accounts");
        });
        let doc = progress.document();
        assert_eq!(doc["phase"], "failed");
        assert_eq!(doc["error"], "boom");
        assert!(doc["percent"].is_null());
    }

    #[test]
    fn the_writer_replaces_the_file_whole() {
        let (progress, _file) = progress_over(b"{}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        {
            let _writer = progress.spawn_writer(path.clone(), Duration::from_millis(20));
            std::thread::sleep(Duration::from_millis(80));
            let doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(doc["phase"], "starting");
            assert_eq!(doc["pid"], std::process::id());
            progress.finish(B256::ZERO);
        }
        // Dropping the writer wrote the final document.
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["phase"], "done");
        assert_eq!(doc["percent"], 100.0);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn key_fractions_span_the_key_space() {
        assert_eq!(key_fraction(B256::ZERO), 0.0);
        let mut half = [0u8; 32];
        half[0] = 0x80;
        assert_eq!(key_fraction(B256::from(half)), 0.5);
        assert!(key_fraction(B256::repeat_byte(0xff)) > 0.9999);
    }
}
