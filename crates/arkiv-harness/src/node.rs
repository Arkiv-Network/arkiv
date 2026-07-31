//! Spawning, killing, and restarting the `arkiv-reth` binary for black-box tests.
//!
//! [`NodeBuilder`] configures a launch; [`Node`] is the running process — killed
//! and its datadir removed on drop. Ports are assigned by the OS at spawn and
//! reused for the node's whole life. [`Node::kill`] stops the process while
//! **keeping** the datadir, and [`Node::restart`] relaunches on the same ports and
//! datadir, so a test can prove crash recovery (state resumes after a restart).

use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// How to launch an `arkiv-reth`. Ports and the datadir are allocated
/// automatically at [`spawn`](NodeBuilder::spawn); everything else has a
/// dev-friendly default you can override.
#[derive(Debug, Clone)]
pub struct NodeBuilder {
    binary: PathBuf,
    dev: bool,
    block_time: String,
    http_api: String,
    extra_args: Vec<String>,
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
            extra_args: Vec::new(),
        }
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

    /// Launch the node.
    pub fn spawn(self) -> Node {
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
            "--disable-discovery",
            // reth's IPC endpoint has a single fixed default path, so nodes
            // running side by side would collide on it. Callers reach the node
            // over HTTP.
            "--ipcdisable",
        ]);
        cmd.args(&self.config.extra_args);

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

    /// Stop the process but **keep** the datadir — the chain persists for a
    /// [`restart`](Node::restart). Idempotent.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
