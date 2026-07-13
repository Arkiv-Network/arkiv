//! End-to-end test: spawn `arkiv-node` in dev mode and prove the whole loop —
//! a client sends a `create` transaction, and reads the entity back over
//! `arkiv_getEntity`.
//!
//! This is black-box: it launches the actual node binary and drives it with alloy,
//! exercising the wired executor (decode → apply → commit), the minting nonce, and
//! the RPC read path against real reth state. It is heavier than the unit tests (it
//! boots a node), but it's the only thing that proves the modules compose live.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, IEntityRegistry, Ident32, Mime128, Operation};
use arkiv_reth_executor::{ARKIV_ADDRESS, derive_entity_key};

/// Test-mnemonic account #0 — the account reth's `--dev` mode pre-funds.
const DEV_PRIVATE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// reth `--dev` chain id.
const CHAIN_ID: u64 = 1337;

/// A running `arkiv-node --dev`, killed and cleaned up on drop.
struct DevNode {
    child: Child,
    port: u16,
    datadir: std::path::PathBuf,
}

/// Hands out a distinct port/datadir offset per [`DevNode`] in this process, so
/// several nodes (across tests in the same binary) don't collide on reth's http,
/// authrpc, or p2p ports.
static NODE_SEQ: AtomicU16 = AtomicU16::new(0);

impl DevNode {
    fn spawn() -> Self {
        // A per-process, per-node offset so nodes never share a port or datadir.
        let seq = NODE_SEQ.fetch_add(1, Ordering::Relaxed);
        let offset = (std::process::id() % 1000) as u16 + seq;
        let port = 18000 + offset; // http
        let authrpc_port = 28000 + offset; // engine API
        let p2p_port = 38000 + offset; // devp2p listener
        let datadir = std::env::temp_dir().join(format!("arkiv-e2e-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&datadir);

        // `arkiv` is intentionally NOT in --http.api: reth rejects unknown modules,
        // and the namespace is merged onto the http transport by extend_rpc_modules.
        let child = Command::new(env!("CARGO_BIN_EXE_arkiv-node"))
            .args([
                "node",
                "--dev",
                "--http",
                "--http.addr",
                "127.0.0.1",
                "--http.port",
                &port.to_string(),
                "--http.api",
                "eth,net,web3,txpool",
                "--authrpc.port",
                &authrpc_port.to_string(),
                "--port",
                &p2p_port.to_string(),
                "--dev.block-time",
                "250ms",
                "--datadir",
                datadir.to_str().unwrap(),
                "--disable-discovery",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn arkiv-node binary");

        Self {
            child,
            port,
            datadir,
        }
    }

    fn http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for DevNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

fn text_plain_mime() -> Mime128 {
    Mime128::encode("text/plain").expect("valid mime")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_then_get_entity_over_a_live_node() {
    let node = DevNode::spawn();

    let signer: PrivateKeySigner = DEV_PRIVATE_KEY.parse().unwrap();
    let caller = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(node.http_url().parse().unwrap());

    // Wait for the node's RPC to come up (debug reth takes a few seconds).
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match provider.get_chain_id().await {
            Ok(_) => break,
            Err(e) => {
                assert!(Instant::now() < deadline, "node RPC not ready in 90s: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    assert_eq!(provider.get_chain_id().await.unwrap(), CHAIN_ID);

    // 1) Send a `create` transaction — exactly what an SDK client does.
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, &provider);
    let op = Operation::create(100, Bytes::from_static(b"hello"), text_plain_mime(), vec![]);
    let receipt = registry
        .execute(vec![op])
        .gas(3_000_000)
        .gas_price(1_000_000_000)
        .send()
        .await
        .expect("send create tx")
        .get_receipt()
        .await
        .expect("get receipt");
    assert!(receipt.status(), "create transaction reverted");

    // 2) The key the node minted for this caller's first create (minting nonce 0),
    //    derived independently — the test never learns it from the node.
    let key = B256::from(derive_entity_key(CHAIN_ID, &caller.into_array(), 0));

    // 3) Read it back over arkiv_getEntity and check the projection.
    let entity: serde_json::Value = provider
        .raw_request("arkiv_getEntity".into(), (key,))
        .await
        .expect("arkiv_getEntity");
    assert!(!entity.is_null(), "entity should exist after create");
    assert_eq!(entity["key"].as_str(), Some(format!("{key:#x}").as_str()));
    assert_eq!(
        entity["owner"].as_str(),
        Some(format!("{caller:#x}").as_str())
    );
    assert_eq!(
        entity["creator"].as_str(),
        Some(format!("{caller:#x}").as_str())
    );
    assert_eq!(entity["contentType"], "text/plain");
    assert_eq!(entity["payload"], "0x68656c6c6f"); // "hello"
    let created = entity["createdAtBlock"].as_u64().unwrap();
    assert_eq!(entity["expiresAt"].as_u64().unwrap(), created + 100); // btl resolved

    // 4) A key that was never created reads back as null.
    let missing: serde_json::Value = provider
        .raw_request("arkiv_getEntity".into(), (B256::repeat_byte(0xAB),))
        .await
        .expect("arkiv_getEntity (missing)");
    assert!(missing.is_null(), "a nonexistent key should read null");
}

/// Block until the node's JSON-RPC answers, or panic after 90s.
async fn wait_for_rpc<P: Provider>(provider: &P) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match provider.get_chain_id().await {
            Ok(_) => break,
            Err(e) => {
                assert!(Instant::now() < deadline, "node RPC not ready in 90s: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// A `rank` (uint) + `team` (string) attribute pair — the two axes the query
/// matrix below filters on.
fn attrs(rank: u64, team: &[u8]) -> Vec<Attribute> {
    vec![
        Attribute::uint(Ident32::encode("rank").unwrap(), U256::from(rank)),
        Attribute::string(Ident32::encode("team").unwrap(), team).unwrap(),
    ]
}

/// Call `arkiv_query` with one page's worth of params.
async fn arkiv_query<P: Provider>(
    provider: &P,
    query: &str,
    page_size: u64,
    cursor: Option<u64>,
) -> serde_json::Value {
    let mut request = serde_json::json!({ "query": query, "pageSize": page_size });
    if let Some(cursor) = cursor {
        request["cursor"] = serde_json::json!(cursor);
    }
    provider
        .raw_request("arkiv_query".into(), (request,))
        .await
        .unwrap_or_else(|e| panic!("arkiv_query {query:?}: {e}"))
}

/// The set of `key` strings in a query response — order-independent, since the
/// index returns newest-first but tests assert on membership.
fn result_keys(response: &serde_json::Value) -> std::collections::BTreeSet<String> {
    response["entities"]
        .as_array()
        .expect("entities array")
        .iter()
        .map(|e| e["key"].as_str().expect("key string").to_string())
        .collect()
}

/// Every query-operator class — eq, IN, ranges, glob, boolean, negation, `$all`,
/// and pagination — against a live node, over the index the executor commits.
///
/// This is the T1 (DB correctness) query surface: five entities are created with
/// `rank`/`team` attributes in one batch, then each operator class is asserted to
/// return exactly the right keys via `arkiv_query`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_operator_classes_over_a_live_node() {
    let node = DevNode::spawn();

    let signer: PrivateKeySigner = DEV_PRIVATE_KEY.parse().unwrap();
    let caller = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(node.http_url().parse().unwrap());
    wait_for_rpc(&provider).await;

    // Five entities in one batch — the i-th create mints its key from nonce i, so
    // the keys are derivable independently (the test never learns them from the node).
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, &provider);
    let payload = Bytes::from_static(b"x");
    let creates = vec![
        Operation::create(1000, payload.clone(), text_plain_mime(), attrs(10, b"red")),
        Operation::create(1000, payload.clone(), text_plain_mime(), attrs(20, b"red")),
        Operation::create(1000, payload.clone(), text_plain_mime(), attrs(30, b"blue")),
        Operation::create(1000, payload.clone(), text_plain_mime(), attrs(40, b"blue")),
        Operation::create(
            1000,
            payload.clone(),
            text_plain_mime(),
            attrs(50, b"green"),
        ),
    ];
    let receipt = registry
        .execute(creates)
        .gas(8_000_000)
        .gas_price(1_000_000_000)
        .send()
        .await
        .expect("send batch create")
        .get_receipt()
        .await
        .expect("get receipt");
    assert!(receipt.status(), "batch create reverted");

    // The five minted keys, derived independently.
    let k: Vec<String> = (0..5)
        .map(|i| {
            let key = B256::from(derive_entity_key(CHAIN_ID, &caller.into_array(), i));
            format!("{key:#x}")
        })
        .collect();
    let expect = |idxs: &[usize]| -> std::collections::BTreeSet<String> {
        idxs.iter().map(|i| k[*i].clone()).collect()
    };

    // eq on a user uint attribute.
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank = 30", 100, None).await),
        expect(&[2]),
    );
    // eq on a built-in field ($owner) — every entity is owned by the caller.
    assert_eq!(
        result_keys(&arkiv_query(&provider, &format!("$owner = {caller:#x}"), 100, None).await),
        expect(&[0, 1, 2, 3, 4]),
    );
    // != (negation).
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank != 30", 100, None).await),
        expect(&[0, 1, 3, 4]),
    );
    // IN.
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank IN (10 50)", 100, None).await),
        expect(&[0, 4]),
    );
    // Numeric ranges.
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank >= 40", 100, None).await),
        expect(&[3, 4]),
    );
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank < 30", 100, None).await),
        expect(&[0, 1]),
    );
    // Glob (string prefix).
    assert_eq!(
        result_keys(&arkiv_query(&provider, "team ~ \"re*\"", 100, None).await),
        expect(&[0, 1]),
    );
    // Boolean AND / OR.
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank >= 20 && team = \"blue\"", 100, None).await),
        expect(&[2, 3]),
    );
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank = 10 || rank = 50", 100, None).await),
        expect(&[0, 4]),
    );
    // $all.
    assert_eq!(
        result_keys(&arkiv_query(&provider, "*", 100, None).await),
        expect(&[0, 1, 2, 3, 4]),
    );

    // Pagination: two per page walks the whole set, honoring next_cursor until it
    // runs out, and the pages partition `$all`.
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = arkiv_query(&provider, "*", 2, cursor).await;
        let keys = result_keys(&page);
        assert!(keys.len() <= 2, "page larger than pageSize");
        seen.extend(keys);
        pages += 1;
        assert!(pages <= 5, "pagination did not terminate");
        match page["nextCursor"].as_u64() {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, expect(&[0, 1, 2, 3, 4]), "pages must partition $all");
    assert_eq!(pages, 3, "5 entities at 2/page is 3 pages");
}

/// Submit `ops` in one `execute` transaction and assert it lands successfully.
async fn execute<P: Provider>(provider: &P, ops: Vec<Operation>) {
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, provider);
    let receipt = registry
        .execute(ops)
        .gas(8_000_000)
        .gas_price(1_000_000_000)
        .send()
        .await
        .expect("send ops")
        .get_receipt()
        .await
        .expect("get receipt");
    assert!(receipt.status(), "transaction reverted");
}

/// `arkiv_getEntity` at the tip.
async fn get_entity<P: Provider>(provider: &P, key: B256) -> serde_json::Value {
    provider
        .raw_request("arkiv_getEntity".into(), (key,))
        .await
        .expect("arkiv_getEntity")
}

/// `arkiv_getEntity` as of a past block.
async fn get_entity_at<P: Provider>(provider: &P, key: B256, block: u64) -> serde_json::Value {
    provider
        .raw_request("arkiv_getEntity".into(), (key, block))
        .await
        .expect("arkiv_getEntity (historical)")
}

/// `arkiv_query` evaluated as of a past block.
async fn arkiv_query_at<P: Provider>(provider: &P, query: &str, block: u64) -> serde_json::Value {
    let request = serde_json::json!({ "query": query, "pageSize": 100, "block": block });
    provider
        .raw_request("arkiv_query".into(), (request,))
        .await
        .unwrap_or_else(|e| panic!("arkiv_query {query:?}@{block}: {e}"))
}

/// `arkiv_getEntityCount` — optional query filter (default `$all`) and block (tip).
async fn entity_count<P: Provider>(provider: &P, query: Option<&str>, block: Option<u64>) -> u64 {
    let mut request = serde_json::json!({});
    if let Some(query) = query {
        request["query"] = serde_json::json!(query);
    }
    if let Some(block) = block {
        request["block"] = serde_json::json!(block);
    }
    let response: serde_json::Value = provider
        .raw_request("arkiv_getEntityCount".into(), (request,))
        .await
        .expect("arkiv_getEntityCount");
    response["count"].as_u64().expect("count u64")
}

/// Every mutating op — update, extend, transfer, delete — driven over a live node
/// and observed through both `arkiv_getEntity` and `arkiv_query`/`arkiv_getEntityCount`.
///
/// Complements the create-only path: proves the executor reindexes on update,
/// moves owner buckets on transfer, and tombstones on delete, all visible over RPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_path_ops_over_a_live_node() {
    let node = DevNode::spawn();

    let signer: PrivateKeySigner = DEV_PRIVATE_KEY.parse().unwrap();
    let caller = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(node.http_url().parse().unwrap());
    wait_for_rpc(&provider).await;

    let key = |i: u32| B256::from(derive_entity_key(CHAIN_ID, &caller.into_array(), i));
    let ks = |i: u32| format!("{:#x}", key(i));

    // Four entities: e0 (update), e1 (extend), e2 (transfer), e3 (delete).
    execute(
        &provider,
        vec![
            Operation::create(
                1000,
                Bytes::from_static(b"v1"),
                text_plain_mime(),
                attrs(10, b"red"),
            ),
            Operation::create(
                1000,
                Bytes::from_static(b"e1"),
                text_plain_mime(),
                attrs(20, b"red"),
            ),
            Operation::create(
                1000,
                Bytes::from_static(b"e2"),
                text_plain_mime(),
                attrs(30, b"blue"),
            ),
            Operation::create(
                1000,
                Bytes::from_static(b"e3"),
                text_plain_mime(),
                attrs(40, b"blue"),
            ),
        ],
    )
    .await;
    assert_eq!(entity_count(&provider, None, None).await, 4);

    // UPDATE e0: new payload + reindexed attributes (rank 10 -> 100).
    execute(
        &provider,
        vec![Operation::update(
            key(0),
            Bytes::from_static(b"v2"),
            text_plain_mime(),
            attrs(100, b"gold"),
        )],
    )
    .await;
    let e0 = get_entity(&provider, key(0)).await;
    assert_eq!(e0["payload"], "0x7632"); // "v2"
    assert!(
        e0["lastModifiedAtBlock"].as_u64().unwrap() > e0["createdAtBlock"].as_u64().unwrap(),
        "update should advance lastModifiedAtBlock",
    );
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank = 100", 100, None).await),
        std::collections::BTreeSet::from([ks(0)]),
        "e0 findable by its new rank",
    );
    assert!(
        result_keys(&arkiv_query(&provider, "rank = 10", 100, None).await).is_empty(),
        "e0's old rank is de-indexed",
    );

    // EXTEND e1: expiry rises.
    let before = get_entity(&provider, key(1)).await["expiresAt"]
        .as_u64()
        .unwrap();
    execute(&provider, vec![Operation::extend(key(1), 5000)]).await;
    let after = get_entity(&provider, key(1)).await["expiresAt"]
        .as_u64()
        .unwrap();
    assert!(
        after > before,
        "extend should raise expiresAt: {before} -> {after}"
    );

    // TRANSFER e2 to a new owner: it changes owner buckets.
    let bob = Address::from([0xBB; 20]);
    execute(&provider, vec![Operation::transfer(key(2), bob)]).await;
    assert_eq!(
        get_entity(&provider, key(2)).await["owner"].as_str(),
        Some(format!("{bob:#x}").as_str()),
    );
    assert_eq!(
        result_keys(&arkiv_query(&provider, &format!("$owner = {bob:#x}"), 100, None).await),
        std::collections::BTreeSet::from([ks(2)]),
        "e2 now in bob's bucket",
    );
    assert!(
        !result_keys(&arkiv_query(&provider, &format!("$owner = {caller:#x}"), 100, None).await)
            .contains(&ks(2)),
        "e2 left the caller's bucket",
    );

    // DELETE e3: gone from getEntity, from $all, and from the count.
    execute(&provider, vec![Operation::delete(key(3))]).await;
    assert!(
        get_entity(&provider, key(3)).await.is_null(),
        "deleted entity reads null",
    );
    assert!(
        !result_keys(&arkiv_query(&provider, "*", 100, None).await).contains(&ks(3)),
        "deleted entity leaves $all",
    );
    assert_eq!(
        entity_count(&provider, None, None).await,
        3,
        "delete drops the count"
    );
}

/// Historical reads (`arkiv_getEntity` / `arkiv_query` as of a past block) and
/// `arkiv_getBlockTiming`, over a live node.
///
/// The Arkiv index lives in ordinary reth state, so a historical state snapshot
/// answers a historical query — this proves that end to end: an entity updated at
/// the tip still reads its original value and matches its original query as of the
/// block it was created in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn historical_reads_and_block_timing_over_a_live_node() {
    let node = DevNode::spawn();

    let signer: PrivateKeySigner = DEV_PRIVATE_KEY.parse().unwrap();
    let caller = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(node.http_url().parse().unwrap());
    wait_for_rpc(&provider).await;

    let key = B256::from(derive_entity_key(CHAIN_ID, &caller.into_array(), 0));
    let ks = format!("{key:#x}");

    // Create the entity (rank 10, payload "v1"), then learn the block it landed in.
    execute(
        &provider,
        vec![Operation::create(
            1000,
            Bytes::from_static(b"v1"),
            text_plain_mime(),
            attrs(10, b"red"),
        )],
    )
    .await;
    let created = get_entity(&provider, key).await["createdAtBlock"]
        .as_u64()
        .unwrap();

    // Update it in a later block (rank 10 -> 100, payload "v1" -> "v2").
    execute(
        &provider,
        vec![Operation::update(
            key,
            Bytes::from_static(b"v2"),
            text_plain_mime(),
            attrs(100, b"red"),
        )],
    )
    .await;

    // The tip reflects the update.
    assert_eq!(get_entity(&provider, key).await["payload"], "0x7632"); // "v2"
    assert_eq!(
        result_keys(&arkiv_query(&provider, "rank = 100", 100, None).await),
        std::collections::BTreeSet::from([ks.clone()]),
    );
    assert!(result_keys(&arkiv_query(&provider, "rank = 10", 100, None).await).is_empty());

    // As of the create block, both the entity and the query see the ORIGINAL state.
    assert_eq!(
        get_entity_at(&provider, key, created).await["payload"],
        "0x7631"
    ); // "v1"
    assert_eq!(
        result_keys(&arkiv_query_at(&provider, "rank = 10", created).await),
        std::collections::BTreeSet::from([ks.clone()]),
        "historical query sees the original rank",
    );
    assert!(
        result_keys(&arkiv_query_at(&provider, "rank = 100", created).await).is_empty(),
        "the updated rank did not exist yet at the create block",
    );

    // Block timing is sane: the tip is at/after the create block, has a real
    // timestamp, and the gap to its predecessor is small (250ms dev blocks).
    let timing = block_timing(&provider).await;
    assert!(timing["current_block"].as_u64().unwrap() >= created);
    assert!(timing["current_block_time"].as_u64().unwrap() > 0);
    assert!(
        timing["duration"].as_u64().unwrap() <= 2,
        "unexpected inter-block duration: {timing}",
    );
}

/// `arkiv_getBlockTiming` at the tip.
async fn block_timing<P: Provider>(provider: &P) -> serde_json::Value {
    provider
        .raw_request("arkiv_getBlockTiming".into(), ())
        .await
        .expect("arkiv_getBlockTiming")
}
