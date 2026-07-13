//! Spawning, killing, and restarting the `arkiv-node` binary for black-box tests.
//!
//! [`NodeBuilder`] configures a launch; [`Node`] is the running process — killed
//! and its datadir removed on drop. [`Node::kill`] stops the process while
//! **keeping** the datadir, and [`Node::restart`] relaunches on the same ports and
//! datadir, so a test can prove crash recovery (state resumes after a restart).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};

/// Per-process, per-node offset so nodes never collide on ports or datadir.
static NODE_SEQ: AtomicU16 = AtomicU16::new(0);

const HTTP_BASE: u16 = 18000;
const AUTHRPC_BASE: u16 = 28000;
const P2P_BASE: u16 = 38000;

/// How to launch an `arkiv-node`. Ports and the datadir are allocated
/// automatically at [`spawn`](NodeBuilder::spawn); everything else has a
/// dev-friendly default you can override.
#[derive(Debug, Clone)]
pub struct NodeBuilder {
    binary: PathBuf,
    dev: bool,
    block_time: String,
    http_api: String,
    jwt_secret: Option<PathBuf>,
    trusted_peers: Vec<String>,
    extra_args: Vec<String>,
}

impl NodeBuilder {
    /// A dev-mode node from the `arkiv-node` binary at `binary` (tests pass
    /// `env!("CARGO_BIN_EXE_arkiv-node")`).
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            dev: true,
            block_time: "250ms".to_string(),
            http_api: "eth,net,web3,txpool".to_string(),
            jwt_secret: None,
            trusted_peers: Vec::new(),
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

    /// Authenticate the Engine API with the JWT secret at `path` (reth reads the
    /// hex file — it must already exist). Two nodes sharing one path share the
    /// secret, so the harness's CL driver can drive the follower's engine.
    pub fn jwt_secret(mut self, path: impl Into<PathBuf>) -> Self {
        self.jwt_secret = Some(path.into());
        self
    }

    /// Add a `--trusted-peers` enode the node dials directly (used to point a
    /// follower at the sequencer, since discovery is disabled).
    pub fn trusted_peer(mut self, enode: impl Into<String>) -> Self {
        self.trusted_peers.push(enode.into());
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

/// A running `arkiv-node`. Killed and its datadir removed on drop.
pub struct Node {
    config: NodeBuilder,
    http_port: u16,
    authrpc_port: u16,
    p2p_port: u16,
    datadir: PathBuf,
    child: Option<Child>,
}

impl Node {
    fn start(config: NodeBuilder) -> Self {
        // A per-process, per-node offset so nodes never share a port or datadir.
        let seq = NODE_SEQ.fetch_add(1, Ordering::Relaxed);
        let offset = (std::process::id() % 1000) as u16 + seq;
        let datadir =
            std::env::temp_dir().join(format!("arkiv-harness-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&datadir);

        let mut node = Self {
            config,
            http_port: HTTP_BASE + offset,
            authrpc_port: AUTHRPC_BASE + offset,
            p2p_port: P2P_BASE + offset,
            datadir,
            child: None,
        };
        node.child = Some(node.spawn_process());
        node
    }

    fn spawn_process(&self) -> Child {
        // `arkiv` is intentionally NOT in --http.api: reth rejects unknown modules,
        // and the namespace is merged onto the http transport by extend_rpc_modules.
        let mut cmd = Command::new(&self.config.binary);
        cmd.arg("node");
        // Always the dev genesis, so a `--dev` sequencer and a plain follower share
        // one chainspec (matching genesis is what lets them peer). `--dev` adds only
        // the auto-seal miner on top.
        cmd.args(["--chain", "dev"]);
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
        ]);
        if let Some(jwt) = &self.config.jwt_secret {
            cmd.args(["--authrpc.jwtsecret", jwt.to_str().expect("utf-8 jwt path")]);
        }
        for peer in &self.config.trusted_peers {
            cmd.args(["--trusted-peers", peer]);
        }
        cmd.args(&self.config.extra_args);
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn arkiv-node binary")
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

    /// The authenticated Engine API URL (for the harness's CL driver).
    pub fn authrpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.authrpc_port)
    }

    /// The devp2p listener port.
    pub fn p2p_port(&self) -> u16 {
        self.p2p_port
    }

    /// The datadir (persists across [`kill`](Node::kill)/[`restart`](Node::restart)).
    pub fn datadir(&self) -> &Path {
        &self.datadir
    }

    /// Whether the process is currently running.
    pub fn is_running(&self) -> bool {
        self.child.is_some()
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

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}
