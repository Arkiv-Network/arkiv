//! A minimal consensus-layer driver — just enough Engine API to make a follower
//! node replicate a sequencer's chain.
//!
//! Post-merge, a follower does **not** sync a live tip over devp2p on its own: a
//! CL has to drive its fork choice. [`EngineClient`] is that CL, reduced to one
//! call — point the follower at the sequencer's head via
//! `engine_forkchoiceUpdatedV3` and let reth backfill the missing blocks from its
//! peer. This is exactly what Lighthouse does to an EL that is behind; the harness
//! stands in for it so replication is testable without a full beacon stack (and a
//! real Lighthouse can later drive the same engine port).

use std::path::Path;
use std::time::{Duration, Instant};

use alloy_primitives::B256;
use alloy_provider::Provider;
use alloy_rpc_client::{ClientBuilder, RpcClient};
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, JwtSecret};
use alloy_transport_http::{AuthLayer, Http, HyperClient};

use crate::client::ArkivClient;

/// Create a fresh JWT secret at `path` (32 random bytes, hex-encoded — the
/// `jwt.hex` format reth's `--authrpc.jwtsecret` reads) and return its hex, so
/// the same secret can be handed to both the node and [`EngineClient`].
pub fn write_jwt_secret(path: &Path) -> String {
    JwtSecret::try_create_random(path).expect("write jwt secret");
    std::fs::read_to_string(path).expect("read jwt secret")
}

/// A JWT-authenticated Engine API client bound to one node's `authrpc` port.
pub struct EngineClient {
    client: RpcClient,
}

impl EngineClient {
    /// Connect to the authenticated Engine API at `authrpc_url`, signing every
    /// request with the shared `jwt_hex` secret (as produced by
    /// [`write_jwt_secret`]). [`AuthLayer`] re-mints the bearer token per request
    /// so it never falls outside the engine's ±60s `iat` window.
    pub fn new(authrpc_url: &str, jwt_hex: &str) -> Self {
        let secret = JwtSecret::from_hex(jwt_hex).expect("valid jwt hex");
        let url = authrpc_url.parse().expect("valid authrpc url");
        // The AuthLayer signs HTTP requests, so it wraps the hyper service inside
        // the transport (not the RPC client), re-minting the bearer per request.
        let hyper = HyperClient::new().layer(AuthLayer::new(secret));
        let transport = Http::with_client(hyper, url);
        let is_local = transport.guess_local();
        let client = ClientBuilder::default().transport(transport, is_local);
        Self { client }
    }

    /// Drive the node's fork choice to `head` (used as head, safe, and finalized),
    /// with no payload attributes — a pure "follow this head" signal. Returns the
    /// engine's status (`SYNCING` while it is still backfilling, `VALID` once the
    /// head is in its canonical chain).
    pub async fn forkchoice_updated(&self, head: B256) -> ForkchoiceUpdated {
        let state = ForkchoiceState::same_hash(head);
        self.client
            .request("engine_forkchoiceUpdatedV3", (state, Option::<()>::None))
            .await
            .expect("engine_forkchoiceUpdatedV3")
    }
}

/// Drive `follower` to replicate `sequencer` up to at least block `target`:
/// repeatedly point the follower's fork choice at the sequencer's live head and
/// let it backfill over P2P. Panics if `target` is not reached within `timeout`.
///
/// The sequencer keeps sealing, so we track its moving head rather than a fixed
/// hash — callers assert determinism at a fixed height afterwards (compare
/// [`block_hash_at`](ArkivClient::block_hash_at) on both nodes).
pub async fn sync_follower<S: Provider, F: Provider>(
    engine: &EngineClient,
    sequencer: &ArkivClient<S>,
    follower: &ArkivClient<F>,
    target: u64,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        engine.forkchoice_updated(sequencer.tip_hash().await).await;
        if follower.block_number().await >= target {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "follower did not replicate to block {target} in {timeout:?}",
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
