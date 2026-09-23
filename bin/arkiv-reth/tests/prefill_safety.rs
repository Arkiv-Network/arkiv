//! Regression tests for the startup and retry hazards found in the v0.3.0 review.

use alloy_primitives::{B256, keccak256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use arkiv_harness::{DEV_CHAIN_ID, DEV_KEY_0, connect_reader};
use arkiv_seed::{SeedSpec, StreamSink, export};
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(120);
const COUNT: u64 = 40;

struct Fixture {
    dir: tempfile::TempDir,
    genesis: PathBuf,
    dump: PathBuf,
    root: B256,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("state.jsonl");
        let owner = DEV_KEY_0.parse::<PrivateKeySigner>().unwrap().address();
        let mut spec = SeedSpec::new(DEV_CHAIN_ID, vec![owner]);
        spec.count = COUNT;
        spec.payload_size = 32;
        spec.expires_at = 1_000_000;
        let mut sink = StreamSink::create(&dump, dir.path().join("sort")).unwrap();
        let manifest = arkiv_seed::build(
            &spec,
            arkiv_genesis::genesis_alloc().unwrap(),
            &mut sink,
            |_| {},
        )
        .unwrap();
        let genesis = dir.path().join("genesis.json");
        fs::write(
            &genesis,
            export::genesis_with_state_hash(
                export::with_snapshot(
                    export::dev_genesis(DEV_CHAIN_ID),
                    &manifest.authenticated_state,
                )
                .unwrap(),
                manifest.state_root,
            )
            .unwrap()
            .to_string(),
        )
        .unwrap();
        Self {
            dir,
            genesis,
            dump,
            root: manifest.state_root,
        }
    }

    fn command(&self, subcommand: &str, datadir: &Path, v2: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arkiv-reth"));
        command
            .arg(subcommand)
            .arg("--chain")
            .arg(&self.genesis)
            .arg("--datadir")
            .arg(datadir)
            .args(["--storage.v2", if v2 { "true" } else { "false" }])
            .args(["--log.file.max-files", "0", "--color", "never"]);
        command
    }

    fn import(&self, datadir: &Path, dump: &Path, v2: bool) -> (ExitStatus, String) {
        let mut command = self.command("init-state", datadir, v2);
        command.arg(dump);
        Process::spawn(command).wait()
    }

    fn node(&self, datadir: &Path, dev: bool, v2: bool) -> (Process, String) {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let auth = TcpListener::bind("127.0.0.1:0").unwrap();
        let p2p = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = http.local_addr().unwrap().port().to_string();
        let mut command = self.command("node", datadir, v2);
        command.args([
            "--http",
            "--http.addr",
            "127.0.0.1",
            "--http.port",
            &port,
            "--authrpc.addr",
            "127.0.0.1",
            "--authrpc.port",
            &auth.local_addr().unwrap().port().to_string(),
            "--port",
            &p2p.local_addr().unwrap().port().to_string(),
            "--disable-discovery",
            "--trusted-only",
            "--max-outbound-peers",
            "0",
            "--ipcdisable",
        ]);
        if dev {
            command.args(["--dev", "--dev.block-time", "500ms"]);
        }
        drop((http, auth, p2p));
        (Process::spawn(command), format!("http://127.0.0.1:{port}"))
    }
}

/// Always reap subprocesses, including when a startup unexpectedly succeeds or
/// a test panics. Files avoid filling a pipe while waiting for process exit.
struct Process {
    child: Child,
    log: tempfile::NamedTempFile,
}

impl Process {
    fn spawn(mut command: Command) -> Self {
        let log = tempfile::NamedTempFile::new().unwrap();
        let child = command
            .stdout(Stdio::from(log.as_file().try_clone().unwrap()))
            .stderr(Stdio::from(log.as_file().try_clone().unwrap()))
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log.path()).unwrap()
    }

    fn wait(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status, self.log());
            }
            assert!(
                Instant::now() < deadline,
                "process did not exit:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    async fn ready(&mut self, url: &str) {
        let client = connect_reader(url);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "node exited:\n{}",
                self.log()
            );
            if client.provider().get_chain_id().await.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "node not ready:\n{}", self.log());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn kill(mut self) {
        // Match the harness's crash-recovery lifecycle. Optimized reth binaries
        // can abort inside RocksDB on SIGINT with two CPUs, including before this
        // fix; that separate shutdown issue is recorded in the safety analysis.
        self.child.kill().unwrap();
        let (_, log) = self.wait();
        assert!(!log.contains("Persistence service failed"), "{log}");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_failure((status, log): (ExitStatus, String), message: &str) {
    assert!(!status.success(), "unexpected success:\n{log}");
    assert!(log.contains(message), "expected {message:?}:\n{log}");
}

fn changesets(datadir: &Path) -> BTreeMap<String, B256> {
    fs::read_dir(datadir.join("static_files"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("change-sets")
        })
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                keccak256(fs::read(path).unwrap()),
            )
        })
        .collect()
}

#[test]
fn skipped_import_and_plain_init_cannot_start_seeded_nodes() {
    let f = Fixture::new();
    for v2 in [true, false] {
        let datadir = f.dir.path().join(format!("skipped-{v2}"));
        for dev in [true, false] {
            assert_failure(
                f.node(&datadir, dev, v2).0.wait(),
                "genesis state import is not complete",
            );
        }
        let (status, log) = Process::spawn(f.command("init", &datadir, v2)).wait();
        assert!(status.success(), "{log}");
        // A stale or forged monitoring file is not an import completion record.
        fs::write(
            datadir.join("init-state-progress.json"),
            r#"{"phase":"done","percent":100}"#,
        )
        .unwrap();
        assert_failure(
            f.node(&datadir, true, v2).0.wait(),
            "genesis state import is not complete",
        );
        // Starting too soon did not poison the datadir: its first import still works.
        let (status, log) = f.import(&datadir, &f.dump, v2);
        assert!(status.success(), "{log}");
    }
}

#[test]
fn failed_computed_root_blocks_startup_and_reimport() {
    let f = Fixture::new();
    let truncated = f.dir.path().join("truncated.jsonl");
    let original = fs::read_to_string(&f.dump).unwrap();
    fs::write(
        &truncated,
        original.lines().take(21).collect::<Vec<_>>().join("\n") + "\n",
    )
    .unwrap();
    for v2 in [true, false] {
        let datadir = f.dir.path().join(format!("failed-{v2}"));
        let (status, log) = f.import(&datadir, &truncated, v2);
        assert!(!status.success(), "{log}");
        assert!(
            log.contains("Computed state root does not match"),
            "must reach the post-write check:\n{log}"
        );
        for dev in [true, false] {
            assert_failure(
                f.node(&datadir, dev, v2).0.wait(),
                "genesis state import is not complete",
            );
        }
        let before = changesets(&datadir);
        assert_failure(
            f.import(&datadir, &f.dump, v2),
            "genesis import was already attempted",
        );
        assert_eq!(changesets(&datadir), before);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_retries_preserve_history_and_successful_startup() {
    let f = Fixture::new();
    let datadir = f.dir.path().join("valid");
    let (status, log) = f.import(&datadir, &f.dump, true);
    assert!(status.success(), "{log}");
    let before = changesets(&datadir);
    assert_eq!(
        before.len(),
        8,
        "both changeset segments and their sidecars must exist"
    );
    let wrong = f.dir.path().join("wrong-root.jsonl");
    fs::write(
        &wrong,
        format!("{{\"root\":\"{}\"}}\n", B256::repeat_byte(0x42)),
    )
    .unwrap();
    assert_failure(f.import(&datadir, &wrong, true), "does not match");
    assert_eq!(changesets(&datadir), before);
    assert_failure(
        f.import(&datadir, &f.dump, true),
        "genesis import was already attempted",
    );
    assert_eq!(changesets(&datadir), before);
    let (status, _) = f.import(&datadir, &f.dir.path().join("missing.jsonl"), true);
    assert!(!status.success());
    assert_eq!(changesets(&datadir), before);

    for restart in 0..2 {
        let (mut node, url) = f.node(&datadir, true, true);
        node.ready(&url).await;
        let client = connect_reader(&url);
        assert_eq!(client.entity_count(None, Some(0)).await, COUNT);
        let tip = client.block_number().await;
        if restart > 0 {
            assert!(tip > 0, "block history must survive the forced stop");
        }
        // As in recovery.rs, advance beyond the in-memory tip before a forced
        // stop so the restart must recover persisted blocks, not only genesis.
        client.wait_for_block(tip + 20, TIMEOUT).await;
        let block = client
            .provider()
            .get_block_by_number(1.into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block.header.state_root, f.root);
        node.kill();
    }
}

#[test]
fn invalid_dump_header_is_retryable_before_import_begins() {
    let f = Fixture::new();
    let datadir = f.dir.path().join("bad-header");
    let bad = f.dir.path().join("bad.jsonl");
    fs::write(&bad, "not json\n").unwrap();
    assert_failure(
        f.import(&datadir, &bad, true),
        "does not start with its state root",
    );
    let before = changesets(&datadir);
    fs::write(&bad, format!("{{\"root\":\"{}\"}}\n", B256::ZERO)).unwrap();
    assert_failure(f.import(&datadir, &bad, true), "does not match");
    assert_eq!(changesets(&datadir), before);
    let (status, log) = f.import(&datadir, &f.dump, true);
    assert!(status.success(), "{log}");
}
