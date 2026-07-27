//! End-to-end tests: spawn the real `arkiv-node` binary and drive it like a
//! client, through the [`arkiv_harness`] node + RPC-client layer.
//!
//! Black-box: it exercises the wired executor (decode → apply → commit), the
//! minting nonce, the `arkiv_*` read path, ownership/atomicity rules, expiry, and
//! historical reads — all against real reth state over JSON-RPC.

use std::collections::BTreeSet;
use std::time::Duration;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::{Attribute, IEntityRegistry, Ident32, Mime128, OP_CREATE, Operation};
use arkiv_harness::{
    ARKIV_ADDRESS, ArkivClient, DEV_CHAIN_ID, DEV_KEY_0, DEV_KEY_1, NodeBuilder, connect,
    derive_entity_key, result_keys,
};

/// Node-boot readiness: up to this many respawns, each given this long to answer
/// RPC (debug reth under parallel node-boot load occasionally wedges on start).
const READY_ATTEMPTS: usize = 3;
const READY_PER_ATTEMPT: Duration = Duration::from_secs(45);

/// Spawn an `arkiv-node --dev` and a signing client for `key`, ready to drive.
async fn spawn_dev(
    key: &str,
) -> (
    arkiv_harness::Node,
    ArkivClient<impl Provider + Clone>,
    Address,
) {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-node")).spawn();
    let signer: PrivateKeySigner = key.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    // Respawn on a stalled boot: many debug-reth nodes booting in parallel across
    // the suite occasionally wedge one past a fixed timeout.
    client
        .wait_ready_resilient(&mut node, READY_ATTEMPTS, READY_PER_ATTEMPT)
        .await;
    (node, client, caller)
}

fn text_plain_mime() -> Mime128 {
    Mime128::encode("text/plain").expect("valid mime")
}

/// A `rank` (uint) + `team` (string) attribute pair — the two axes the query
/// matrix filters on.
fn attrs(rank: u64, team: &[u8]) -> Vec<Attribute> {
    vec![
        Attribute::uint(Ident32::encode("rank").unwrap(), U256::from(rank)),
        Attribute::string(Ident32::encode("team").unwrap(), team).unwrap(),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_then_get_entity_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    assert_eq!(client.chain_id().await, DEV_CHAIN_ID);

    // 1) Send a `create` transaction — exactly what an SDK client does.
    let op = Operation::create(100, Bytes::from_static(b"hello"), text_plain_mime(), vec![]);
    let receipt = client.execute(vec![op]).await;

    // 2) The key the node minted for this caller's first create (minting nonce 0),
    //    derived independently — the test never learns it from the node.
    let key = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 0));

    // 2a) The create emits exactly one EntityOperation log at ARKIV_ADDRESS,
    //     carrying the minted key/owner/type — the event surface off-chain
    //     indexers consume.
    let events: Vec<_> = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.inner.address == ARKIV_ADDRESS)
        .map(|log| {
            IEntityRegistry::EntityOperation::decode_log(&log.inner)
                .expect("decode EntityOperation log")
        })
        .collect();
    assert_eq!(events.len(), 1, "one EntityOperation log for one create");
    assert_eq!(events[0].entityKey, key);
    assert_eq!(events[0].operationType, OP_CREATE);
    assert_eq!(events[0].owner, caller);

    // 3) Read it back over arkiv_getEntity and check the projection.
    let entity = client.get_entity(key).await;
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
    let missing = client.get_entity(B256::repeat_byte(0xAB)).await;
    assert!(missing.is_null(), "a nonexistent key should read null");
}

/// The `nonces(address)` view over `eth_call` — the exact call SDKs make
/// before a create to predict the keys the batch will mint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nonces_view_tracks_creates_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());

    // Fresh accounts have minting nonce 0, whoever asks.
    assert_eq!(registry.nonces(caller).call().await.unwrap(), 0);
    let stranger = Address::repeat_byte(0xCD);
    assert_eq!(registry.nonces(stranger).call().await.unwrap(), 0);

    // Two creates in one batch advance the caller's minting nonce by two.
    let op = || Operation::create(100, Bytes::from_static(b"n"), text_plain_mime(), vec![]);
    client.execute(vec![op(), op()]).await;
    assert_eq!(registry.nonces(caller).call().await.unwrap(), 2);
    assert_eq!(registry.nonces(stranger).call().await.unwrap(), 0);
}

/// Business-rule failures surface as ABI-encoded `IEntityRegistry` errors in
/// the revert data — decodable by the SDK — not as plain strings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_revert_errors_over_a_live_node() {
    use alloy_sol_types::{SolCall, SolError};

    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    // Revert data of an eth_call to ARKIV_ADDRESS, from the JSON-RPC error's
    // `data` field.
    let revert_of = |ops: Vec<Operation>, from: Address| {
        let client = &client;
        async move {
            let calldata = IEntityRegistry::executeCall { ops }.abi_encode();
            let err = client
                .provider()
                .raw_request::<_, Bytes>(
                    "eth_call".into(),
                    (
                        serde_json::json!({
                            "from": from,
                            "to": ARKIV_ADDRESS,
                            "data": format!("0x{}", alloy_primitives::hex::encode(&calldata)),
                        }),
                        "latest",
                    ),
                )
                .await
                .expect_err("call must revert");
            err.to_string()
        }
    };

    // Update on a key that was never created → EntityNotFound(key).
    let ghost = B256::repeat_byte(0x42);
    let update = Operation::update(ghost, Bytes::from_static(b"x"), text_plain_mime(), vec![]);
    let err = revert_of(vec![update], caller).await;
    assert!(
        err.contains(&alloy_primitives::hex::encode(
            IEntityRegistry::EntityNotFound::SELECTOR
        )),
        "expected EntityNotFound revert data, got: {err}"
    );

    // A stranger updating a real entity → NotOwner(key, caller, owner).
    let op = Operation::create(100, Bytes::from_static(b"v1"), text_plain_mime(), vec![]);
    client.execute(vec![op]).await;
    let key = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 0));
    let stranger = Address::repeat_byte(0xCD);
    let update = Operation::update(key, Bytes::from_static(b"v2"), text_plain_mime(), vec![]);
    let err = revert_of(vec![update], stranger).await;
    assert!(
        err.contains(&alloy_primitives::hex::encode(
            IEntityRegistry::NotOwner::SELECTOR
        )),
        "expected NotOwner revert data, got: {err}"
    );

    // An empty batch → EmptyBatch().
    let err = revert_of(vec![], caller).await;
    assert!(
        err.contains(&alloy_primitives::hex::encode(
            IEntityRegistry::EmptyBatch::SELECTOR
        )),
        "expected EmptyBatch revert data, got: {err}"
    );

    // An invalid attribute name (uppercase bytes, as an SDK sends for
    // "testInvalidKey") → Ident32InvalidByte(position, value).
    let mut name = [0u8; 32];
    name[..14].copy_from_slice(b"testInvalidKey");
    let bad_attr = Attribute {
        name: alloy_primitives::FixedBytes::from(name).into(),
        valueType: 2, // ATTR_STRING
        value: [alloy_primitives::FixedBytes::ZERO; 4],
    };
    let create = Operation::create(
        100,
        Bytes::from_static(b"x"),
        text_plain_mime(),
        vec![bad_attr],
    );
    let err = revert_of(vec![create], caller).await;
    assert!(
        err.contains(&alloy_primitives::hex::encode(
            IEntityRegistry::Ident32InvalidByte::SELECTOR
        )),
        "expected Ident32InvalidByte revert data, got: {err}"
    );
}

/// The SDK's gas flow: eth_estimateGas, then send with exactly that limit. The
/// estimate must clear the tx-pool's intrinsic floor even for ops the cost
/// model prices below it (update's 40k base vs create's 80k — the case that
/// used to be rejected as IntrinsicGasTooLow).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn estimated_gas_is_accepted_by_the_pool() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());

    // Payload large enough that the EIP-7623 floor (≈40 gas/nonzero byte)
    // overtakes the cost model's 16/byte for an update.
    let payload = Bytes::from(vec![0xABu8; 2_000]);

    // Create, estimate-then-send — no explicit gas: alloy fills it from
    // eth_estimateGas, exactly like the SDK.
    let create = Operation::create(1000, payload.clone(), text_plain_mime(), vec![]);
    let receipt = registry
        .execute(vec![create])
        .send()
        .await
        .expect("create with estimated gas accepted")
        .get_receipt()
        .await
        .expect("create receipt");
    assert!(receipt.status(), "create must succeed");

    // Update the same entity — the op that used to estimate below the floor.
    let key = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 0));
    let update = Operation::update(key, payload, text_plain_mime(), vec![]);
    let receipt = registry
        .execute(vec![update])
        .send()
        .await
        .expect("update with estimated gas accepted by the pool")
        .get_receipt()
        .await
        .expect("update receipt");
    assert!(receipt.status(), "update must succeed");
}

/// Every query-operator class — eq, IN, ranges, glob, boolean, negation, built-in
/// fields, `$all`, pagination — plus the RPC error contract, against a live node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_operator_classes_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    // Five entities in one batch — the i-th create mints its key from nonce i.
    let payload = Bytes::from_static(b"x");
    client
        .execute(vec![
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
        ])
        .await;

    let k: Vec<String> = (0..5)
        .map(|i| {
            format!(
                "{:#x}",
                B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), i))
            )
        })
        .collect();
    let expect =
        |idxs: &[usize]| -> BTreeSet<String> { idxs.iter().map(|i| k[*i].clone()).collect() };

    // eq (user uint), eq ($owner), !=, IN, ranges, glob, AND/OR, $all.
    assert_eq!(
        result_keys(&client.query("rank = 30", 100, None).await),
        expect(&[2])
    );
    assert_eq!(
        result_keys(
            &client
                .query(&format!("$owner = {caller:#x}"), 100, None)
                .await
        ),
        expect(&[0, 1, 2, 3, 4]),
    );
    assert_eq!(
        result_keys(&client.query("rank != 30", 100, None).await),
        expect(&[0, 1, 3, 4])
    );
    assert_eq!(
        result_keys(&client.query("rank IN (10 50)", 100, None).await),
        expect(&[0, 4])
    );
    assert_eq!(
        result_keys(&client.query("rank >= 40", 100, None).await),
        expect(&[3, 4])
    );
    assert_eq!(
        result_keys(&client.query("rank < 30", 100, None).await),
        expect(&[0, 1])
    );
    assert_eq!(
        result_keys(&client.query("team ~ \"re*\"", 100, None).await),
        expect(&[0, 1])
    );
    assert_eq!(
        result_keys(
            &client
                .query("rank >= 20 && team = \"blue\"", 100, None)
                .await
        ),
        expect(&[2, 3]),
    );
    assert_eq!(
        result_keys(&client.query("rank = 10 || rank = 50", 100, None).await),
        expect(&[0, 4])
    );
    assert_eq!(
        result_keys(&client.query("*", 100, None).await),
        expect(&[0, 1, 2, 3, 4])
    );

    // Pagination: two per page partitions $all over three pages. The cursor is
    // an opaque hex string echoed back verbatim.
    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = client.query("*", 2, cursor.as_deref()).await;
        let keys = result_keys(&page);
        assert!(keys.len() <= 2, "page larger than resultsPerPage");
        seen.extend(keys);
        pages += 1;
        assert!(pages <= 5, "pagination did not terminate");
        match page["cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, expect(&[0, 1, 2, 3, 4]), "pages must partition $all");
    assert_eq!(pages, 3, "5 entities at 2/page is 3 pages");

    // Built-in fields beyond $owner (all share creator/content-type/create-block).
    assert_eq!(
        result_keys(
            &client
                .query(&format!("$creator = {caller:#x}"), 100, None)
                .await
        ),
        expect(&[0, 1, 2, 3, 4]),
    );
    assert_eq!(
        result_keys(
            &client
                .query("$contentType = \"text/plain\"", 100, None)
                .await
        ),
        expect(&[0, 1, 2, 3, 4]),
    );
    assert_eq!(
        result_keys(&client.query("$createdAtBlock >= 1", 100, None).await),
        expect(&[0, 1, 2, 3, 4]),
    );

    // Negation operators.
    assert_eq!(
        result_keys(&client.query("rank NOT IN (10 50)", 100, None).await),
        expect(&[1, 2, 3])
    );
    assert_eq!(
        result_keys(&client.query("team !~ \"re*\"", 100, None).await),
        expect(&[2, 3, 4])
    );
    assert_eq!(
        result_keys(&client.query("NOT (rank = 30)", 100, None).await),
        expect(&[0, 1, 3, 4])
    );

    // Error contract: malformed query, out-of-range block, bad atBlock tag.
    assert!(
        client
            .query_raw("rank = = 30", serde_json::json!({}))
            .await
            .is_err(),
        "malformed query must error",
    );
    assert!(
        client
            .query_raw("*", serde_json::json!({ "atBlock": "0x5f5e0ff" }))
            .await
            .is_err(),
        "a future block must error",
    );
    assert!(
        client
            .query_raw("*", serde_json::json!({ "atBlock": "pending" }))
            .await
            .is_err(),
        "non-latest tags must error",
    );

    // resultsPerPage 0 clamps to 1 rather than erroring (branch/SDK semantics).
    let clamped = client
        .query_raw("*", serde_json::json!({ "resultsPerPage": 0 }))
        .await
        .expect("resultsPerPage 0 clamps");
    assert_eq!(result_keys(&clamped).len(), 1);

    // SDK wire shapes: a bare query string with no options object (the exact
    // form viem sends for key lookups), hex resultsPerPage, and includeData
    // projection.
    let bare: serde_json::Value = client
        .provider()
        .raw_request("arkiv_query".into(), (format!("$key = {}", k[0]),))
        .await
        .expect("bare-string arkiv_query");
    assert_eq!(result_keys(&bare), expect(&[0]));
    assert!(
        bare["blockNumber"]
            .as_str()
            .is_some_and(|b| b.starts_with("0x")),
        "blockNumber is a hex string"
    );

    let hex_page = client
        .query_raw("*", serde_json::json!({ "resultsPerPage": "0x2" }))
        .await
        .expect("hex resultsPerPage");
    assert_eq!(result_keys(&hex_page).len(), 2);

    let projected = client
        .query_raw(
            &format!("$key = {}", k[0]),
            serde_json::json!({ "includeData": { "key": true } }),
        )
        .await
        .expect("includeData projection");
    let entity = &projected["data"][0];
    assert!(entity["key"].as_str().is_some(), "key requested → present");
    assert!(
        entity.get("value").is_none(),
        "value not requested → absent"
    );
    assert!(
        entity.get("owner").is_none(),
        "owner not requested → absent"
    );
}

/// Every mutating op — update, extend, transfer, delete — driven over a live node
/// and observed through `arkiv_getEntity`, `arkiv_query`, and `arkiv_getEntityCount`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_path_ops_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    let key = |i: u32| B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), i));
    let ks = |i: u32| format!("{:#x}", key(i));

    // Four entities: e0 (update), e1 (extend), e2 (transfer), e3 (delete).
    client
        .execute(vec![
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
        ])
        .await;
    assert_eq!(client.entity_count(None, None).await, 4);

    // UPDATE e0: new payload + reindexed attributes (rank 10 -> 100).
    client
        .execute(vec![Operation::update(
            key(0),
            Bytes::from_static(b"v2"),
            text_plain_mime(),
            attrs(100, b"gold"),
        )])
        .await;
    let e0 = client.get_entity(key(0)).await;
    assert_eq!(e0["payload"], "0x7632"); // "v2"
    assert!(
        e0["lastModifiedAtBlock"].as_u64().unwrap() > e0["createdAtBlock"].as_u64().unwrap(),
        "update should advance lastModifiedAtBlock",
    );
    assert_eq!(
        result_keys(&client.query("rank = 100", 100, None).await),
        BTreeSet::from([ks(0)]),
        "e0 findable by its new rank",
    );
    assert!(
        result_keys(&client.query("rank = 10", 100, None).await).is_empty(),
        "e0's old rank is de-indexed",
    );

    // EXTEND e1: expiry rises.
    let before = client.get_entity(key(1)).await["expiresAt"]
        .as_u64()
        .unwrap();
    client.execute(vec![Operation::extend(key(1), 5000)]).await;
    let after = client.get_entity(key(1)).await["expiresAt"]
        .as_u64()
        .unwrap();
    assert!(
        after > before,
        "extend should raise expiresAt: {before} -> {after}"
    );

    // TRANSFER e2 to a new owner: it changes owner buckets.
    let bob = Address::from([0xBB; 20]);
    client.execute(vec![Operation::transfer(key(2), bob)]).await;
    assert_eq!(
        client.get_entity(key(2)).await["owner"].as_str(),
        Some(format!("{bob:#x}").as_str()),
    );
    assert_eq!(
        result_keys(&client.query(&format!("$owner = {bob:#x}"), 100, None).await),
        BTreeSet::from([ks(2)]),
        "e2 now in bob's bucket",
    );
    assert!(
        !result_keys(
            &client
                .query(&format!("$owner = {caller:#x}"), 100, None)
                .await
        )
        .contains(&ks(2)),
        "e2 left the caller's bucket",
    );

    // DELETE e3: gone from getEntity, from $all, and from the count.
    client.execute(vec![Operation::delete(key(3))]).await;
    assert!(
        client.get_entity(key(3)).await.is_null(),
        "deleted entity reads null"
    );
    assert!(
        !result_keys(&client.query("*", 100, None).await).contains(&ks(3)),
        "deleted entity leaves $all",
    );
    assert_eq!(
        client.entity_count(None, None).await,
        3,
        "delete drops the count"
    );
}

/// Historical reads (`arkiv_getEntity` / `arkiv_query` as of a past block) and
/// `arkiv_getBlockTiming`: an entity updated at the tip still reads its original
/// value, and matches its original query, as of the block it was created in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn historical_reads_and_block_timing_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    let key = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 0));
    let ks = format!("{key:#x}");

    client
        .execute(vec![Operation::create(
            1000,
            Bytes::from_static(b"v1"),
            text_plain_mime(),
            attrs(10, b"red"),
        )])
        .await;
    let created = client.get_entity(key).await["createdAtBlock"]
        .as_u64()
        .unwrap();

    client
        .execute(vec![Operation::update(
            key,
            Bytes::from_static(b"v2"),
            text_plain_mime(),
            attrs(100, b"red"),
        )])
        .await;

    // The tip reflects the update.
    assert_eq!(client.get_entity(key).await["payload"], "0x7632"); // "v2"
    assert_eq!(
        result_keys(&client.query("rank = 100", 100, None).await),
        BTreeSet::from([ks.clone()]),
    );
    assert!(result_keys(&client.query("rank = 10", 100, None).await).is_empty());

    // As of the create block, both the entity and the query see the ORIGINAL state.
    assert_eq!(
        client.get_entity_at(key, created).await["payload"],
        "0x7631"
    ); // "v1"
    assert_eq!(
        result_keys(&client.query_at("rank = 10", created).await),
        BTreeSet::from([ks.clone()]),
        "historical query sees the original rank",
    );
    assert!(
        result_keys(&client.query_at("rank = 100", created).await).is_empty(),
        "the updated rank did not exist yet at the create block",
    );

    // Block timing is sane.
    let timing = client.block_timing().await;
    assert!(timing["current_block"].as_u64().unwrap() >= created);
    assert!(timing["current_block_time"].as_u64().unwrap() > 0);
    assert!(
        timing["duration"].as_u64().unwrap() <= 2,
        "unexpected inter-block duration: {timing}",
    );
}

/// Ownership is enforced and batches are atomic: a non-owner's writes revert
/// leaving state untouched, and a batch with one failing op rolls back wholesale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthorized_ops_and_batch_atomicity_over_a_live_node() {
    let (node, owner, owner_addr) = spawn_dev(DEV_KEY_0).await;

    // A second, also-prefunded account as a non-owner, on the same node.
    let stranger_signer: PrivateKeySigner = DEV_KEY_1.parse().unwrap();
    let stranger = connect(&node.http_url(), stranger_signer);

    owner
        .execute(vec![Operation::create(
            1000,
            Bytes::from_static(b"v1"),
            text_plain_mime(),
            vec![],
        )])
        .await;
    let key = B256::from(derive_entity_key(DEV_CHAIN_ID, &owner_addr.into_array(), 0));
    assert_eq!(owner.get_entity(key).await["payload"], "0x7631"); // "v1"

    // The stranger cannot update or delete the owner's entity — both revert.
    assert!(
        !stranger
            .try_execute(vec![Operation::update(
                key,
                Bytes::from_static(b"hacked"),
                text_plain_mime(),
                vec![],
            )])
            .await,
        "non-owner update must revert",
    );
    assert!(
        !stranger.try_execute(vec![Operation::delete(key)]).await,
        "non-owner delete must revert",
    );
    assert_eq!(
        owner.get_entity(key).await["payload"],
        "0x7631",
        "entity unchanged after failed writes",
    );

    // Batch atomicity: a batch whose second op reverts rolls back the whole tx —
    // the create in the same batch does not persist.
    let phantom = B256::repeat_byte(0xCD);
    assert!(
        !owner
            .try_execute(vec![
                Operation::create(
                    1000,
                    Bytes::from_static(b"batch"),
                    text_plain_mime(),
                    vec![]
                ),
                Operation::delete(phantom),
            ])
            .await,
        "a batch with a failing op must revert wholesale",
    );
    let would_be = B256::from(derive_entity_key(DEV_CHAIN_ID, &owner_addr.into_array(), 1));
    assert!(
        owner.get_entity(would_be).await.is_null(),
        "the reverted batch minted no entity",
    );
    assert_eq!(
        owner.entity_count(None, None).await,
        1,
        "count unchanged by the reverted batch"
    );
}

/// The explicit `expire` op: rejected while the entity is live, accepted once the
/// chain passes its expiry block, after which the entity is gone from reads,
/// queries, and the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_expire_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    // Short TTL so the expiry block arrives quickly (250ms blocks).
    client
        .execute(vec![Operation::create(
            10,
            Bytes::from_static(b"ttl"),
            text_plain_mime(),
            vec![],
        )])
        .await;
    let key = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 0));
    let expires_at = client.get_entity(key).await["expiresAt"].as_u64().unwrap();

    // While live, `expire` reverts.
    assert!(
        !client.try_execute(vec![Operation::expire(key)]).await,
        "a live entity cannot be expired",
    );
    assert!(!client.get_entity(key).await.is_null(), "still live");

    // Once the chain passes the expiry block, `expire` succeeds and the entity is
    // gone everywhere.
    client
        .wait_for_block(expires_at + 1, Duration::from_secs(30))
        .await;
    assert!(
        client.try_execute(vec![Operation::expire(key)]).await,
        "expire past the TTL succeeds",
    );
    assert!(
        client.get_entity(key).await.is_null(),
        "expired entity reads null"
    );
    assert!(
        !result_keys(&client.query("*", 100, None).await).contains(&format!("{key:#x}")),
        "expired entity leaves queries",
    );
    assert_eq!(client.entity_count(None, None).await, 0);
}
