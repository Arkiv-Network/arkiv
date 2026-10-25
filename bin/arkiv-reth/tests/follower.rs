//! A GolemDB follower kept in step with a producer through the Engine API.
//!
//! The test plays the consensus client. A producer runs in `--dev` and mines
//! blocks; a follower runs without `--dev`, so it only advances when the test
//! sends it `engine_newPayload` and `engine_forkchoiceUpdated`, as Lighthouse
//! would. Both nodes run on GolemDB from one genesis file, so their genesis
//! hashes and state roots agree, and the follower has the producer as a
//! trusted peer to download blocks from.
//!
//! GolemDB saves a block only once it is canonical, so a follower can execute a
//! block only on top of the canonical head. The scenarios cover the ways a
//! follower meets blocks whose parent is not saved yet: a block delivered
//! before its parent is the head, and a gap the follower closes by
//! downloading.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alloy_consensus::TxEnvelope;
use alloy_primitives::B256;
use alloy_provider::Provider;
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::{Claims, ExecutionPayloadV3, JwtSecret};
use alloy_signer_local::PrivateKeySigner;
use arkiv_harness::{DEV_KEY_0, Node, NodeBuilder, connect_reader};
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
use serde_json::{Value, json};

/// How long to wait for a freshly spawned node's RPC to answer (debug reth is slow).
const READY: Duration = Duration::from_secs(90);

/// How long the follower has to reach a target head once it is told about it.
const CATCH_UP: Duration = Duration::from_secs(240);

/// How long the producer has to mine the blocks a gap needs.
const MINING: Duration = Duration::from_secs(180);

/// How long one Engine API call may take. Executing a block waits for its
/// parent's state, so a call that runs out names a follower that is stuck.
const ENGINE_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a waiting test asks again.
const POLL: Duration = Duration::from_millis(500);

/// The short gap, in blocks: below reth's 32-block backfill threshold.
const SHORT_GAP: u64 = 30;

/// The long gap, in blocks: past reth's 32-block backfill threshold, and over
/// three times the follower's block buffer.
const LONG_GAP: u64 = 200;

/// The follower's block buffer limit, far below the long gap.
const BLOCK_BUFFER_LIMIT: &str = "64";

/// What the follower's log must never say: GolemDB refusing a block whose
/// parent it has not saved, and a wait for adoption that ran out.
const FORBIDDEN_LOG_LINES: [&str; 2] = ["not in the Arkiv store", "adoption has not finished"];

/// The Engine API secret both sides of the follower's authenticated RPC share.
const JWT_SECRET: &str = "4f5c3a3d9e6b2a17c8d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6";

/// A GolemDB follower driven through the Engine API catches up with a
/// producer: a block delivered before its parent is the head waits instead of
/// failing, and gaps of 30 and 200 blocks after a restart are downloaded and
/// executed one canonical block at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golemdb_follower_catches_up_over_the_engine_api() {
    let genesis = genesis_file();
    let jwt = jwt_file();

    let mut follower = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .dev(false)
        .chain(&genesis)
        .golemdb_store()
        .http_api("eth,net,web3,admin")
        .arg("--authrpc.jwtsecret")
        .arg(jwt.to_str().expect("utf-8 path"))
        .arg("--engine.block-buffer-limit")
        .arg(BLOCK_BUFFER_LIMIT)
        .spawn();
    let follower_rpc = connect_reader(&follower.http_url());
    follower.wait_ready(&follower_rpc, READY).await;

    let mut producer = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis)
        .golemdb_store()
        .http_api("eth,net,web3,txpool,admin")
        .spawn();
    let producer_rpc = connect_reader(&producer.http_url());
    producer.wait_ready(&producer_rpc, READY).await;
    let producer_enode = enode(producer_rpc.provider(), &producer).await;

    let engine = EngineClient::new(follower.authrpc_port());
    add_trusted_peer(follower_rpc.provider(), &producer_enode).await;

    // Out-of-order delivery: block 2 arrives before block 1 is the head.
    producer_rpc.wait_for_block(2, MINING).await;
    let block_1 = producer_block(producer_rpc.provider(), 1).await;
    let block_2 = producer_block(producer_rpc.provider(), 2).await;
    assert_eq!(engine.new_payload(&block_1).await, "VALID", "block 1");
    assert_eq!(
        engine.new_payload(&block_2).await,
        "SYNCING",
        "block 2 waits for block 1 to become the head"
    );
    assert_eq!(engine.forkchoice_updated(block_1.hash).await, "VALID");
    assert_eq!(
        engine.forkchoice_updated(block_2.hash).await,
        "VALID",
        "block 2 executed once block 1 became the head"
    );
    assert_head(follower_rpc.provider(), &block_2).await;
    assert_clean_log(&follower);

    // A short gap after a restart.
    let target =
        restart_after_gap(&mut follower, producer_rpc.provider(), SHORT_GAP, &block_2).await;
    follower.wait_ready(&follower_rpc, READY).await;
    add_trusted_peer(follower_rpc.provider(), &producer_enode).await;
    let target = catch_up(
        &engine,
        follower_rpc.provider(),
        producer_rpc.provider(),
        &target,
    )
    .await;
    assert_clean_log(&follower);

    // A long gap after a restart: past reth's backfill threshold, and larger
    // than the follower's block buffer.
    let target = restart_after_gap(&mut follower, producer_rpc.provider(), LONG_GAP, &target).await;
    follower.wait_ready(&follower_rpc, READY).await;
    add_trusted_peer(follower_rpc.provider(), &producer_enode).await;
    catch_up(
        &engine,
        follower_rpc.provider(),
        producer_rpc.provider(),
        &target,
    )
    .await;
    assert_clean_log(&follower);

    let _ = std::fs::remove_file(&genesis);
    let _ = std::fs::remove_file(&jwt);
}

/// A block of the producer's, as the follower is sent it.
struct ProducerBlock {
    number: u64,
    hash: B256,
    payload: ExecutionPayloadV3,
    parent_beacon_block_root: B256,
}

/// Block `number` of the producer's chain, as an execution payload.
async fn producer_block(provider: &impl Provider, number: u64) -> ProducerBlock {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .full()
        .await
        .expect("eth_getBlockByNumber")
        .unwrap_or_else(|| panic!("the producer has block {number}"));
    let hash = block.header.hash;
    let block = block
        .map_transactions(|tx| tx.into_inner())
        .into_consensus();
    let parent_beacon_block_root = block
        .header
        .parent_beacon_block_root
        .expect("a Cancun header carries the parent beacon block root");
    ProducerBlock {
        number,
        hash,
        payload: ExecutionPayloadV3::from_block_unchecked::<TxEnvelope, _>(hash, &block),
        parent_beacon_block_root,
    }
}

/// Stop the follower, let the producer mine `gap` blocks past `last`, the
/// follower's head, and start the follower again. Returns the producer's block
/// at the end of the gap, the target the follower is then pointed at.
async fn restart_after_gap(
    follower: &mut Node,
    producer: &impl Provider,
    gap: u64,
    last: &ProducerBlock,
) -> ProducerBlock {
    follower.terminate();
    let target = last.number + gap;
    let deadline = Instant::now() + MINING;
    while producer.get_block_number().await.expect("block number") < target {
        assert!(
            Instant::now() < deadline,
            "the producer did not reach block {target} in {MINING:?}"
        );
        tokio::time::sleep(POLL).await;
    }
    follower.restart();
    producer_block(producer, target).await
}

/// Point the follower at `target` the way a consensus client does, repeating
/// the forkchoice update until it answers VALID, and check that the follower's
/// head is then the producer's block.
///
/// Then deliver the producer's next block as well. Executing it needs
/// `target`'s state in GolemDB, so it proves the gap was saved block by block
/// rather than only made canonical.
async fn catch_up(
    engine: &EngineClient,
    follower: &impl Provider,
    producer: &impl Provider,
    target: &ProducerBlock,
) -> ProducerBlock {
    let started = Instant::now();
    let deadline = started + CATCH_UP;
    loop {
        let status = engine.forkchoice_updated(target.hash).await;
        if status == "VALID" {
            break;
        }
        assert_eq!(status, "SYNCING", "the follower is catching up");
        assert!(
            Instant::now() < deadline,
            "the follower did not reach block {} in {CATCH_UP:?}; it is at block {}",
            target.number,
            follower.get_block_number().await.unwrap_or_default(),
        );
        tokio::time::sleep(POLL).await;
    }
    eprintln!(
        "the follower reached block {} in {:?}",
        target.number,
        started.elapsed()
    );
    assert_head(follower, target).await;

    let next = next_producer_block(producer, target).await;
    assert_eq!(
        engine.new_payload(&next).await,
        "VALID",
        "the block after the gap executes on its state"
    );
    assert_eq!(engine.forkchoice_updated(next.hash).await, "VALID");
    assert_head(follower, &next).await;
    next
}

/// The producer's block after `block`, once it has mined it.
async fn next_producer_block(producer: &impl Provider, block: &ProducerBlock) -> ProducerBlock {
    let number = block.number + 1;
    let deadline = Instant::now() + MINING;
    while producer.get_block_number().await.expect("block number") < number {
        assert!(
            Instant::now() < deadline,
            "the producer did not reach block {number} in {MINING:?}"
        );
        tokio::time::sleep(POLL).await;
    }
    producer_block(producer, number).await
}

/// The follower's head is the producer's `block`.
async fn assert_head(follower: &impl Provider, block: &ProducerBlock) {
    let head = follower
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
        .expect("eth_getBlockByNumber")
        .expect("the follower has a head");
    assert_eq!(head.header.number, block.number, "the follower's head");
    assert_eq!(
        head.header.hash, block.hash,
        "the follower's head is the producer's block {}",
        block.number
    );
}

/// The follower's log, since its last start, has none of [`FORBIDDEN_LOG_LINES`].
fn assert_clean_log(follower: &Node) {
    let log = std::fs::read_to_string(follower.log_path()).expect("read the follower's log");
    for line in FORBIDDEN_LOG_LINES {
        assert!(
            !log.contains(line),
            "the follower's log says {line:?}:\n{}",
            log.lines()
                .filter(|l| l.contains(line))
                .take(5)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// The producer's enode URL, on the loopback address the follower dials.
async fn enode(provider: &impl Provider, producer: &Node) -> String {
    let info: Value = provider
        .raw_request("admin_nodeInfo".into(), ())
        .await
        .expect("admin_nodeInfo");
    let enode = info["enode"].as_str().expect("an enode URL");
    let id = enode
        .strip_prefix("enode://")
        .and_then(|rest| rest.split('@').next())
        .expect("enode://<id>@<address>");
    format!("enode://{id}@127.0.0.1:{}", producer.p2p_port())
}

/// Make the producer a trusted peer of the follower. Repeated after each
/// restart, since a node keeps trusted peers added over RPC only while it runs.
async fn add_trusted_peer(follower: &impl Provider, enode: &str) {
    let added: bool = follower
        .raw_request("admin_addTrustedPeer".into(), (enode,))
        .await
        .expect("admin_addTrustedPeer");
    assert!(added, "the follower takes the producer as a trusted peer");
}

/// The follower's authenticated Engine API, called as a consensus client
/// calls it, with a fresh JWT per request.
struct EngineClient {
    url: String,
    secret: JwtSecret,
    http: reqwest::Client,
}

impl EngineClient {
    fn new(port: u16) -> Self {
        Self {
            url: format!("http://127.0.0.1:{port}"),
            secret: JwtSecret::from_hex(JWT_SECRET).expect("a 32-byte hex secret"),
            http: reqwest::Client::builder()
                .timeout(ENGINE_CALL_TIMEOUT)
                .build()
                .expect("an HTTP client"),
        }
    }

    /// Call `method`, returning its result, or panicking with its error.
    async fn call(&self, method: &str, params: Value) -> Value {
        let token = self
            .secret
            .encode(&Claims::default())
            .expect("sign the JWT");
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(request.to_string())
            .send()
            .await
            .unwrap_or_else(|e| {
                if e.is_timeout() {
                    panic!("the follower did not answer {method} in {ENGINE_CALL_TIMEOUT:?}")
                }
                panic!("{method}: {e}")
            })
            .text()
            .await
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        let response: Value =
            serde_json::from_str(&response).unwrap_or_else(|e| panic!("{method}: {e}: {response}"));
        if let Some(error) = response.get("error") {
            panic!("{method} failed: {error}");
        }
        response["result"].clone()
    }

    /// `engine_newPayloadV4` for `block`, returning the payload status.
    async fn new_payload(&self, block: &ProducerBlock) -> String {
        let result = self
            .call(
                "engine_newPayloadV4",
                json!([
                    block.payload,
                    Vec::<B256>::new(),
                    block.parent_beacon_block_root,
                    Vec::<String>::new()
                ]),
            )
            .await;
        status(&result)
    }

    /// `engine_forkchoiceUpdatedV3` with `head` as the head and no payload to
    /// build, returning the payload status.
    async fn forkchoice_updated(&self, head: B256) -> String {
        let state = json!({
            "headBlockHash": head,
            "safeBlockHash": B256::ZERO,
            "finalizedBlockHash": B256::ZERO,
        });
        let result = self
            .call("engine_forkchoiceUpdatedV3", json!([state, Value::Null]))
            .await;
        status(&result["payloadStatus"])
    }
}

/// The `status` of an Engine API payload status.
fn status(payload_status: &Value) -> String {
    payload_status["status"]
        .as_str()
        .unwrap_or_else(|| panic!("a payload status: {payload_status}"))
        .to_owned()
}

/// A genesis file for both nodes: every fork from block 0, [`DEV_KEY_0`]
/// funded, and GolemDB's settings, so block 0 carries GolemDB's root.
fn genesis_file() -> PathBuf {
    let funded = DEV_KEY_0.parse::<PrivateKeySigner>().unwrap().address();
    let genesis = json!({
        "config": {
            "chainId": 424243,
            "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
            "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "mergeNetsplitBlock": 0, "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0,
            "arkiv": { "golemdb": {
                "hash_function": "keccak-256",
                "cell_limits": {
                    "max_cell_name_len": MAX_ATTRIBUTE_NAME_BYTES,
                    "max_str_len": MAX_STR_BYTES,
                    "max_bytes_len": MAX_BYTES_VALUE_LEN
                },
                "record_limits": {
                    "max_cells": MAX_RECORD_CELLS,
                    "max_indexed_cells": MAX_RECORD_INDEXED_CELLS
                }
            } }
        },
        "gasLimit": "0x3938700",
        "difficulty": "0x0",
        "alloc": { format!("{funded:#x}"): { "balance": "0xd3c21bcecceda1000000" } }
    });
    let path = unique_temp_file("genesis", "json");
    std::fs::write(&path, genesis.to_string()).expect("write genesis");
    path
}

/// A file holding [`JWT_SECRET`], for the follower's `--authrpc.jwtsecret`.
fn jwt_file() -> PathBuf {
    let path = unique_temp_file("jwt", "hex");
    std::fs::write(&path, JWT_SECRET).expect("write the JWT secret");
    path
}

/// A path in the temporary directory that no other test process uses.
fn unique_temp_file(name: &str, extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "arkiv-follower-{name}-{}-{}.{extension}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
