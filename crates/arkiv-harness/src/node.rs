//! Spawning, killing, and restarting the `arkiv-reth` binary for black-box tests.
//!
//! [`NodeBuilder`] configures a launch; [`Node`] is the running process — killed
//! and its datadir removed on drop. Ports are assigned by the OS at spawn and
//! reused for the node's whole life. [`Node::kill`] stops the process while
//! **keeping** the datadir, and [`Node::restart`] relaunches on the same ports and
//! datadir, so a test can prove crash recovery (state resumes after a restart).
//!
//! The datadir is only reth's half. Arkiv's own state lives in the store, which
//! by default is the in-process one (`memory`) and so goes with the process —
//! a test that wants to watch state survive a restart needs
//! [`NodeBuilder::golemdb_store`].

use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use alloy_provider::Provider;

use crate::ArkivClient;

/// Per-process, per-node counter so nodes never share a datadir or log file.
static NODE_SEQ: AtomicU16 = AtomicU16::new(0);

/// How much of the node's log a panicking test gets to see.
const LOG_TAIL_LINES: usize = 150;

/// How often [`Node::wait_ready`] checks the process and its RPC.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long [`Node::terminate`] lets a node finish its own shutdown before
/// resorting to SIGKILL.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// How to launch an `arkiv-reth`. Ports and the datadir are allocated
/// automatically at [`spawn`](NodeBuilder::spawn); everything else has a
/// dev-friendly default you can override.
#[derive(Debug, Clone)]
pub struct NodeBuilder {
    binary: PathBuf,
    dev: bool,
    block_time: String,
    http_api: String,
    chain: Option<PathBuf>,
    extra_args: Vec<String>,
    env: Vec<(String, String)>,
    /// Whether this node's Arkiv state is in GolemDB, inside its datadir.
    golemdb: bool,
}

impl NodeBuilder {
    /// A dev-mode node from the `arkiv-reth` binary at `binary` (tests pass
    /// `env!("CARGO_BIN_EXE_arkiv-reth")`).
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            dev: true,
            block_time: "250ms".to_string(),
            http_api: "eth,net,web3,txpool".to_string(),
            chain: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            golemdb: false,
        }
    }

    /// Run on the genesis file at `path` (`--chain`) instead of the built-in
    /// dev chain. Dev mode still auto-seals on it.
    pub fn chain(mut self, path: impl Into<PathBuf>) -> Self {
        self.chain = Some(path.into());
        self
    }

    /// Toggle `--dev` (auto-sealing sequencer). Off = a follower that only
    /// advances when driven (P2P/engine).
    pub fn dev(mut self, on: bool) -> Self {
        self.dev = on;
        self
    }

    /// The `--dev.block-time` (only applied in dev mode).
    pub fn block_time(mut self, block_time: impl Into<String>) -> Self {
        self.block_time = block_time.into();
        self
    }

    /// The `--http.api` module list.
    pub fn http_api(mut self, http_api: impl Into<String>) -> Self {
        self.http_api = http_api.into();
        self
    }

    /// Append one extra CLI argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    /// Set one environment variable on the node process, for both the first
    /// launch and every [`restart`](Node::restart).
    ///
    /// This is how a test chooses the node's backing store:
    /// `ARKIV_EXPERIMENTAL_STORE` and `ARKIV_STORE_NAMESPACE` are read by the
    /// binary at startup, and
    /// setting them per node rather than on the test process keeps nodes
    /// running side by side out of each other's state.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Put the node's Arkiv state in GolemDB, in a directory inside its datadir.
    ///
    /// It survives a kill and a restart, and it goes with the datadir when the
    /// [`Node`] drops.
    pub fn golemdb_store(mut self) -> Self {
        self.golemdb = true;
        self
    }

    /// Launch the node.
    pub fn spawn(self) -> Node {
        assert!(
            !(self.golemdb
                && self
                    .env
                    .iter()
                    .any(|(key, _)| key == "ARKIV_EXPERIMENTAL_STORE")),
            "golemdb_store() and an ARKIV_EXPERIMENTAL_STORE: a node has one store",
        );
        Node::start(self)
    }
}

/// A running `arkiv-reth`. Killed and its datadir and log removed on drop; a
/// drop while the thread is panicking prints the tail of the log first.
pub struct Node {
    config: NodeBuilder,
    http_port: u16,
    authrpc_port: u16,
    p2p_port: u16,
    datadir: PathBuf,
    log_path: PathBuf,
    child: Option<Child>,
}

impl Node {
    fn start(config: NodeBuilder) -> Self {
        // A per-process, per-node sequence number so nodes never share a datadir.
        let seq = NODE_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("arkiv-harness-{}-{seq}", std::process::id());
        let datadir = std::env::temp_dir().join(&name);
        // A sibling of the datadir, so wiping the datadir mid-run keeps the log.
        let log_path = std::env::temp_dir().join(format!("{name}.log"));
        let _ = std::fs::remove_dir_all(&datadir);

        // Three ports the OS certifies as free. Their listeners hold them until
        // the node is about to bind, so no other process can take one meanwhile.
        let (http, http_port) = reserve_port();
        let (authrpc, authrpc_port) = reserve_port();
        let (p2p, p2p_port) = reserve_port();

        let mut node = Self {
            config,
            http_port,
            authrpc_port,
            p2p_port,
            datadir,
            log_path,
            child: None,
        };
        drop((http, authrpc, p2p));
        node.child = Some(node.spawn_process());
        node
    }

    fn spawn_process(&self) -> Child {
        // `arkiv` is intentionally NOT in --http.api: reth rejects unknown modules,
        // and the namespace is merged onto the http transport by extend_rpc_modules.
        let mut cmd = Command::new(&self.config.binary);
        cmd.arg("node");
        if self.config.dev {
            cmd.arg("--dev")
                .args(["--dev.block-time", &self.config.block_time]);
        }
        if let Some(chain) = &self.config.chain {
            cmd.arg("--chain").arg(chain);
        }
        let log_dir = self.datadir.join("logs");
        cmd.args([
            "--http",
            "--http.addr",
            "127.0.0.1",
            "--http.port",
            &self.http_port.to_string(),
            "--http.api",
            &self.config.http_api,
            "--authrpc.port",
            &self.authrpc_port.to_string(),
            "--port",
            &self.p2p_port.to_string(),
            "--datadir",
            self.datadir.to_str().expect("utf-8 datadir"),
            // Inside this node's datadir, not reth's shared default. Every
            // node otherwise writes to one `reth.log` and rotates it under
            // the others' feet -- shared mutable state between tests that are
            // supposed to be independent, and it does not go away by running
            // them one at a time.
            "--log.file.directory",
            log_dir.to_str().expect("utf-8 log directory"),
            "--disable-discovery",
            // reth's IPC endpoint has a single fixed default path, so nodes
            // running side by side would collide on it. Callers reach the node
            // over HTTP.
            "--ipcdisable",
        ]);
        cmd.args(&self.config.extra_args);
        // The node refuses to start without a store. Tests run on the
        // in-process one unless the test process names another. A node has one
        // store, so a GolemDB path the test process carries is not passed on.
        if std::env::var_os("ARKIV_EXPERIMENTAL_STORE").is_none() {
            cmd.env("ARKIV_EXPERIMENTAL_STORE", "memory");
        }
        cmd.env_remove("ARKIV_STORE_PATH");
        cmd.envs(self.config.env.iter().map(|(k, v)| (k, v)));
        if self.config.golemdb {
            cmd.env("ARKIV_STORE_PATH", self.datadir.join("golemdb"))
                .env_remove("ARKIV_EXPERIMENTAL_STORE");
        }
        // An escape hatch for diagnosing a node under test: extra flags without
        // editing the test that spawns it. Used for raising the log filter when
        // a run's behaviour, not its assertions, is the thing in question.
        if let Ok(extra) = std::env::var("ARKIV_HARNESS_NODE_ARGS") {
            cmd.args(extra.split_whitespace());
        }

        // Both streams go to one file, freshly truncated, so its tail is the
        // current run's output.
        let log = File::create(&self.log_path).expect("create node log");
        let log_err = log.try_clone().expect("clone node log handle");
        cmd.stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("spawn arkiv-reth binary")
    }

    /// Print the tail of the node's log to stderr, so a failing test's captured
    /// output carries the node's own account of the run. Best-effort: a log that
    /// cannot be read simply goes unreported.
    fn dump_log(&self) {
        let Ok(log) = std::fs::read_to_string(&self.log_path) else {
            return;
        };
        let lines: Vec<&str> = log.lines().collect();
        let from = lines.len().saturating_sub(LOG_TAIL_LINES);
        eprintln!(
            "── node on port {} — last {} of {} log lines ({}) ──",
            self.http_port,
            lines.len() - from,
            lines.len(),
            self.log_path.display(),
        );
        for line in &lines[from..] {
            eprintln!("{line}");
        }
    }

    /// The node's HTTP JSON-RPC URL.
    pub fn http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }

    /// The HTTP JSON-RPC port.
    pub fn http_port(&self) -> u16 {
        self.http_port
    }

    /// The authenticated Engine API port.
    pub fn authrpc_port(&self) -> u16 {
        self.authrpc_port
    }

    /// The devp2p listener port.
    pub fn p2p_port(&self) -> u16 {
        self.p2p_port
    }

    /// The datadir (persists across [`kill`](Node::kill)/[`restart`](Node::restart)).
    pub fn datadir(&self) -> &Path {
        &self.datadir
    }

    /// The file holding the node's stdout and stderr for the current process.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Whether the process is currently running.
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Block until `client` gets an answer to `eth_chainId`, or panic — as soon
    /// as the process exits, or after `timeout`.
    ///
    /// Watching the process turns a node that dies at startup into an immediate
    /// failure naming its exit status, rather than a wait for the full timeout.
    /// The log tail the panic-time [`Drop`] prints carries the node's own reason.
    pub async fn wait_ready(&mut self, client: &ArkivClient<impl Provider>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut().expect("node is running");
            if let Ok(Some(status)) = child.try_wait() {
                panic!("node exited during startup ({status})");
            }
            match client.provider().get_chain_id().await {
                Ok(_) => return,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "node RPC not ready in {timeout:?}: {e}"
                    );
                    tokio::time::sleep(READY_POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Block until the process exits and return its status, or panic after
    /// `timeout`. For a node that is meant to refuse to start; its reason is in
    /// [`log_path`](Node::log_path).
    pub async fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut().expect("node is running");
            if let Some(status) = child.try_wait().expect("check the node process") {
                self.child = None;
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "node still running after {timeout:?}"
            );
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }

    /// Stop the process abruptly (SIGKILL) but **keep** the datadir — the chain
    /// persists for a [`restart`](Node::restart). Idempotent.
    ///
    /// This is a crash, not a shutdown: the node gets no chance to flush, so
    /// reth's own database can be left mid-write. For the restart an operator
    /// would perform, use [`terminate`](Node::terminate) — the two are not
    /// interchangeable, and a node can survive one and not the other.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Stop the process the way an operator would — SIGTERM, so reth runs its
    /// shutdown — and **keep** the datadir for a [`restart`](Node::restart).
    ///
    /// Falls back to [`kill`](Node::kill) if the node has not exited within
    /// [`SHUTDOWN_GRACE`], so a node that hangs cannot hang the test.
    pub fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // `std::process::Child` can only SIGKILL, and reaching for a signal
        // crate to send one other signal is not worth it.
        let _ = Command::new("kill").arg(child.id().to_string()).status();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(READY_POLL_INTERVAL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Relaunch on the same ports and datadir. Must [`kill`](Node::kill) first.
    pub fn restart(&mut self) {
        assert!(self.child.is_none(), "kill the node before restart");
        self.child = Some(self.spawn_process());
    }
}

/// Take a free port from the OS. The listener holds the port until it is
/// dropped, which is what makes the port exclusively this node's.
fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("listener address").port();
    (listener, port)
}

impl Drop for Node {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.dump_log();
        }
        self.kill();
        let _ = std::fs::remove_dir_all(&self.datadir);
        let _ = std::fs::remove_file(&self.log_path);
    }
}
