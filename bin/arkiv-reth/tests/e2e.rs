//! End-to-end tests: spawn the real `arkiv-reth` binary and drive it like a
//! client, through the [`arkiv_harness`] node + RPC-client layer.
//!
//! Black-box: it exercises the wired executor (decode → apply → commit), the
//! minting nonce, the `arkiv_*` read path, ownership/atomicity rules, expiry, and
//! historical reads — all against real reth state over JSON-RPC.

use std::collections::BTreeSet;
use std::time::Duration;

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::{
    Attribute, AttributeType, AttributeValue, IEntityRegistry, Ident32, Operation,
};
use arkiv_harness::{
    ARKIV_ADDRESS, ArkivClient, DEV_CHAIN_ID, DEV_KEY_0, DEV_KEY_1, EntityNonce, NodeBuilder,
    connect, derive_entity_key, hex_quantity, result_keys,
};

/// How long to wait for a freshly-spawned node's RPC to answer (debug reth is slow).
const READY: Duration = Duration::from_secs(90);

/// Spawn an `arkiv-reth --dev` and a signing client for `key`, ready to drive.
async fn spawn_dev(
    key: &str,
) -> (
    arkiv_harness::Node,
    ArkivClient<impl Provider + Clone>,
    Address,
) {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth")).spawn();
    let signer: PrivateKeySigner = key.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    (node, client, caller)
}

/// A create carrying `payload` under `text/plain`, with a purely relative
/// lifetime — `$contentType` / `$payload` ride in the attribute list now.
fn create_op(min_lifetime: u64, payload: Bytes, mut attrs: Vec<Attribute>) -> Operation {
    attrs.push(
        Attribute::from_value(
            Ident32::system("$contentType").unwrap(),
            &AttributeValue::Str("text/plain".into()),
        )
        .unwrap(),
    );
    attrs.push(
        Attribute::from_value(
            Ident32::system("$payload").unwrap(),
            &AttributeValue::Bytes(payload.to_vec()),
        )
        .unwrap(),
    );
    Operation::create(0, 0, min_lifetime, 0, attrs)
}

/// A patch that replaces just the payload.
fn patch_payload(key: B256, payload: Bytes) -> Operation {
    Operation::patch(
        key,
        vec![
            Attribute::from_value(
                Ident32::system("$payload").unwrap(),
                &AttributeValue::Bytes(payload.to_vec()),
            )
            .unwrap(),
        ],
    )
}

/// A `rank` (uint) + `team` (string) attribute pair — the two axes the query
/// matrix filters on.
fn attrs(rank: u64, team: &str) -> Vec<Attribute> {
    let attr = |name: &str, value: AttributeValue| {
        Attribute::from_value(Ident32::encode(name).unwrap(), &value).unwrap()
    };
    vec![
        attr("rank", AttributeValue::u256_from_u64(rank)),
        attr("team", AttributeValue::Str(team.into())),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_then_get_entity_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    assert_eq!(client.chain_id().await, DEV_CHAIN_ID);

    // 1) Send a `create` transaction — exactly what an SDK client does.
    let op = create_op(100, Bytes::from_static(b"hello"), attrs(7, "red"));
    let receipt = client.execute(vec![op]).await;

    // 2) The key the node minted for this caller's first create (minting nonce 0),
    //    derived independently — the test never learns it from the node.
    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityNonce::new(0),
        0,
    ));

    // 2a) The create emits exactly one EntityOperation log at ARKIV_ADDRESS,
    //     carrying the minted key/owner/type — the event surface off-chain
    //     indexers consume.
    let events: Vec<_> = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.inner.address == ARKIV_ADDRESS)
        .map(|log| {
            IEntityRegistry::EntityCreated::decode_log(&log.inner)
                .expect("decode EntityCreated log")
        })
        .collect();
    assert_eq!(events.len(), 1, "one EntityCreated log for one create");
    assert_eq!(events[0].entityKey, key);
    assert_eq!(events[0].owner, caller);
    assert_eq!(events[0].creationFlags, 0);

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
    let created = hex_quantity(&entity["createdAt"]);
    assert_eq!(hex_quantity(&entity["expiresAt"]), created + 100); // btl resolved
    // Attributes are typed, in the same shape `arkiv_query` answers with.
    assert_eq!(
        entity["attributes"],
        serde_json::json!([
            { "name": "rank", "type": "u256", "value": "0x7" },
            { "name": "team", "type": "str", "value": "red" },
        ])
    );

    // 3a) The two read methods answer with one entity model — an SDK decodes
    //     a result from either the same way.
    let queried = client
        .query_raw(
            &format!("$key = key({key:#x})"),
            serde_json::json!({ "select": {
                "key": true, "owner": true, "creator": true,
                "createdAt": true, "updatedAt": true, "expiresAt": true,
                "creationFlags": true,
                "contentType": true, "payload": true, "attributes": true,
            }}),
        )
        .await
        .expect("query by key");
    assert_eq!(queried["data"][0], entity, "getEntity and query must agree");

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
    assert_eq!(registry.entityNonce(caller).call().await.unwrap(), 0);
    let stranger = Address::repeat_byte(0xCD);
    assert_eq!(registry.entityNonce(stranger).call().await.unwrap(), 0);

    // Two creates in one batch advance the caller's minting nonce by two.
    let op = || create_op(100, Bytes::from_static(b"n"), vec![]);
    client.execute(vec![op(), op()]).await;
    assert_eq!(registry.entityNonce(caller).call().await.unwrap(), 2);
    assert_eq!(registry.entityNonce(stranger).call().await.unwrap(), 0);
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
    let update = patch_payload(ghost, Bytes::from_static(b"x"));
    let err = revert_of(vec![update], caller).await;
    assert!(
        err.contains(&alloy_primitives::hex::encode(
            IEntityRegistry::EntityNotFound::SELECTOR
        )),
        "expected EntityNotFound revert data, got: {err}"
    );

    // A stranger updating a real entity → NotOwner(key, caller, owner).
    let op = create_op(100, Bytes::from_static(b"v1"), vec![]);
    client.execute(vec![op]).await;
    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityNonce::new(0),
        0,
    ));
    let stranger = Address::repeat_byte(0xCD);
    let update = patch_payload(key, Bytes::from_static(b"v2"));
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
        name: alloy_primitives::FixedBytes::from(name),
        typeId: AttributeType::Str.id(),
        value: Bytes::new(),
    };
    let create = create_op(100, Bytes::from_static(b"x"), vec![bad_attr]);
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
    let create = create_op(1000, payload.clone(), vec![]);
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
    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityNonce::new(0),
        0,
    ));
    let update = patch_payload(key, payload);
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
            create_op(1000, payload.clone(), attrs(10, "red")),
            create_op(1000, payload.clone(), attrs(20, "red")),
            create_op(1000, payload.clone(), attrs(30, "blue")),
            create_op(1000, payload.clone(), attrs(40, "blue")),
            create_op(1000, payload.clone(), attrs(50, "green")),
        ])
        .await;

    let k: Vec<String> = (0..5)
        .map(|i| {
            format!(
                "{:#x}",
                B256::from(derive_entity_key(
                    DEV_CHAIN_ID,
                    &caller.into_array(),
                    EntityNonce::new(i),
                    0
                ))
            )
        })
        .collect();
    let expect =
        |idxs: &[usize]| -> BTreeSet<String> { idxs.iter().map(|i| k[*i].clone()).collect() };

    // Typed equality (user u256 and $owner), ranges, STARTSWITH, AND/OR, `*`.
    assert_eq!(
        result_keys(&client.query("rank = u256(30)", 100, None).await),
        expect(&[2])
    );
    assert_eq!(
        result_keys(
            &client
                .query(&format!("$owner = addr({caller:#x})"), 100, None)
                .await
        ),
        expect(&[0, 1, 2, 3, 4]),
    );
    assert_eq!(
        result_keys(
            &client
                .query("rank = u256(10) OR rank = u256(50)", 100, None)
                .await
        ),
        expect(&[0, 4])
    );
    assert_eq!(
        result_keys(&client.query("rank >= u256(40)", 100, None).await),
        expect(&[3, 4])
    );
    assert_eq!(
        result_keys(&client.query("rank < u256(30)", 100, None).await),
        expect(&[0, 1])
    );
    assert_eq!(
        result_keys(&client.query("team STARTSWITH str('re')", 100, None).await),
        expect(&[0, 1])
    );
    assert_eq!(
        result_keys(
            &client
                .query("rank >= u256(20) AND team = str('blue')", 100, None)
                .await
        ),
        expect(&[2, 3]),
    );
    assert_eq!(
        result_keys(&client.query("*", 100, None).await),
        expect(&[0, 1, 2, 3, 4])
    );

    // A predicate asserts the attribute's *type*: `rank` is a u256, so the same
    // number tagged i32 addresses a different bucket and matches nothing.
    assert!(
        result_keys(&client.query("rank = i32(30)", 100, None).await).is_empty(),
        "an i32 predicate must not match a u256 attribute",
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
                .query(&format!("$creator = addr({caller:#x})"), 100, None)
                .await
        ),
        expect(&[0, 1, 2, 3, 4]),
    );
    assert_eq!(
        result_keys(
            &client
                .query("$contentType = str('text/plain')", 100, None)
                .await
        ),
        expect(&[0, 1, 2, 3, 4]),
    );
    // Block heights are u64 and must say so — an untagged number means i32.
    assert_eq!(
        result_keys(&client.query("$createdAt >= u64(1)", 100, None).await),
        expect(&[0, 1, 2, 3, 4]),
    );

    // NOT is the only negation, and it is the full complement.
    assert_eq!(
        result_keys(
            &client
                .query("NOT (rank = u256(10) OR rank = u256(50))", 100, None)
                .await
        ),
        expect(&[1, 2, 3])
    );
    assert_eq!(
        result_keys(
            &client
                .query("NOT team STARTSWITH str('re')", 100, None)
                .await
        ),
        expect(&[2, 3, 4])
    );
    assert_eq!(
        result_keys(&client.query("NOT (rank = u256(30))", 100, None).await),
        expect(&[0, 1, 3, 4])
    );

    // The exact-type rule, end to end: `rank` is stored as u256, so an untagged
    // 30 (which means i32) is a *valid* query that matches nothing — not an
    // error. Getting this wrong silently returns the u256 rows.
    assert!(
        result_keys(&client.query("rank = 30", 100, None).await).is_empty(),
        "an i32 predicate must not match a u256-typed attribute",
    );
    assert_eq!(
        result_keys(&client.query("rank = u256(30)", 100, None).await),
        expect(&[2]),
    );

    // Error contract: each class of failure reaches the caller under its own
    // stable code, because the code is what an SDK branches on.
    for (rejected, code) in [
        ("rank = = u256(30)", "-32001"),                      // malformed
        ("rank >= u256(20) && team = str('blue')", "-32001"), // removed operator
        ("$createdAt >= 1", "-32002"),                        // system u64 needs its tag
        ("rank != u256(30)", "-32002"),                       // != is not in the language
        ("team >= str('blue')", "-32002"),                    // range on an unordered type
        ("exists(rank)", "-32002"),                           // reserved, unimplemented
        ("rank = i32(2147483648)", "-32003"),                 // literal out of range
        (
            "who = addr(0x5aAeb6053f3E94C9b9A09f33669435E7Ef1BeAed)",
            "-32003",
        ), // bad EIP-55 checksum
        (&"(".repeat(200), "-32004"),                         // nested too deeply
    ] {
        let err = client
            .query_raw(rejected, serde_json::json!({}))
            .await
            .expect_err(rejected)
            .to_string();
        assert!(
            err.contains(code),
            "{rejected} should be {code}, got: {err}"
        );
    }
    let future_block = client
        .query_raw("*", serde_json::json!({ "atBlock": "0x5f5e0ff" }))
        .await
        .expect_err("a future block must error")
        .to_string();
    assert!(
        future_block.contains("-32006"),
        "an unavailable block should be -32006, got: {future_block}"
    );
    assert!(
        client
            .query_raw("*", serde_json::json!({ "atBlock": "pending" }))
            .await
            .is_err(),
        "non-latest tags must error",
    );

    // `limit` is bounded rather than silently trimmed, in both directions.
    for bad_limit in [serde_json::json!(0), serde_json::json!(201)] {
        assert!(
            client
                .query_raw("*", serde_json::json!({ "limit": bad_limit }))
                .await
                .is_err(),
            "limit {bad_limit} must error rather than clamp",
        );
    }
    // A typo'd option is refused instead of being quietly ignored.
    assert!(
        client
            .query_raw("*", serde_json::json!({ "resultsPerPage": 2 }))
            .await
            .is_err(),
        "unknown options must error",
    );

    // SDK wire shapes: a bare query string with no options object (the exact
    // form viem sends for key lookups), and a hex `limit`.
    let bare: serde_json::Value = client
        .provider()
        .raw_request("arkiv_query".into(), (format!("$key = key({})", k[0]),))
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
        .query_raw("*", serde_json::json!({ "limit": "0x2" }))
        .await
        .expect("hex limit");
    assert_eq!(result_keys(&hex_page).len(), 2);

    // Projections are opt-in: the default is the key alone.
    let defaulted = client
        .query_raw(&format!("$key = key({})", k[0]), serde_json::json!({}))
        .await
        .expect("default projection");
    let entity = &defaulted["data"][0];
    assert!(entity["key"].as_str().is_some(), "key is the default field");
    assert_eq!(
        entity.as_object().unwrap().len(),
        1,
        "nothing but the key without a select: {entity}"
    );

    // …and each selected field comes back in its spec encoding.
    let projected = client
        .query_raw(
            &format!("$key = key({})", k[0]),
            serde_json::json!({ "select": {
                "key": true, "owner": true, "expiresAt": true,
                "attributes": true, "attributeSchema": true,
            }}),
        )
        .await
        .expect("select projection");
    let entity = &projected["data"][0];
    assert!(entity["owner"].as_str().is_some(), "owner requested");
    assert!(
        entity["expiresAt"]
            .as_str()
            .is_some_and(|b| b.starts_with("0x")),
        "chain quantities are hex: {entity}"
    );
    assert!(entity.get("payload").is_none(), "payload not requested");
    assert!(
        entity.get("creator").is_none(),
        "creator not requested → absent"
    );
    // rank is a u256 (hex quantity) and team a str, both tagged by type name.
    let attributes = entity["attributes"].as_array().expect("attributes array");
    let rank = attributes
        .iter()
        .find(|a| a["name"] == "rank")
        .expect("rank attribute");
    assert_eq!(rank["type"], "u256");
    assert_eq!(rank["value"], "0xa");
    let team = attributes
        .iter()
        .find(|a| a["name"] == "team")
        .expect("team attribute");
    assert_eq!(team["type"], "str");
    assert_eq!(team["value"], "red");
    // The schema is the same names and types, without the values.
    let schema = entity["attributeSchema"]
        .as_array()
        .expect("attributeSchema array");
    assert_eq!(schema.len(), attributes.len());
    assert!(schema.iter().all(|entry| entry.get("value").is_none()));

    // A named subset returns only what was asked for.
    let subset = client
        .query_raw(
            &format!("$key = key({})", k[0]),
            serde_json::json!({ "select": { "attributes": { "team": true } } }),
        )
        .await
        .expect("named attribute subset");
    let names: Vec<_> = subset["data"][0]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["team"]);

    // Cursors are opaque and bound to the request that issued them.
    let first = client.query("*", 2, None).await;
    let cursor = first["cursor"].as_str().expect("a cursor for page 2");
    assert!(cursor.starts_with("b64:"), "opaque cursor: {cursor}");
    assert!(
        client
            .query_raw(
                "rank >= u256(10)",
                serde_json::json!({ "limit": 2, "cursor": cursor }),
            )
            .await
            .is_err(),
        "a cursor from another query must be refused",
    );
    assert!(
        client
            .query_raw("*", serde_json::json!({ "limit": 2, "cursor": "b64:zzzz" }))
            .await
            .is_err(),
        "a malformed cursor must be refused",
    );
}

/// Every mutating op — update, extend, transfer, delete — driven over a live node
/// and observed through `arkiv_getEntity`, `arkiv_query`, and `arkiv_getEntityCount`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_path_ops_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    let key = |i: u64| {
        B256::from(derive_entity_key(
            DEV_CHAIN_ID,
            &caller.into_array(),
            EntityNonce::new(i),
            0,
        ))
    };
    let ks = |i: u64| format!("{:#x}", key(i));

    // Four entities: e0 (update), e1 (extend), e2 (transfer), e3 (delete).
    client
        .execute(vec![
            create_op(1000, Bytes::from_static(b"v1"), attrs(10, "red")),
            create_op(1000, Bytes::from_static(b"e1"), attrs(20, "red")),
            create_op(1000, Bytes::from_static(b"e2"), attrs(30, "blue")),
            create_op(1000, Bytes::from_static(b"e3"), attrs(40, "blue")),
        ])
        .await;
    assert_eq!(client.entity_count(None, None).await, 4);

    // PATCH e0: new payload + reindexed attributes (rank 10 -> 100).
    client
        .execute(vec![{
            let mut muts = attrs(100, "gold");
            muts.push(
                Attribute::from_value(
                    Ident32::system("$payload").unwrap(),
                    &AttributeValue::Bytes(b"v2".to_vec()),
                )
                .unwrap(),
            );
            Operation::patch(key(0), muts)
        }])
        .await;
    let e0 = client.get_entity(key(0)).await;
    assert_eq!(e0["payload"], "0x7632"); // "v2"
    assert!(
        hex_quantity(&e0["updatedAt"]) > hex_quantity(&e0["createdAt"]),
        "patch should advance updatedAt",
    );
    assert_eq!(
        result_keys(&client.query("rank = u256(100)", 100, None).await),
        BTreeSet::from([ks(0)]),
        "e0 findable by its new rank",
    );
    assert!(
        result_keys(&client.query("rank = u256(10)", 100, None).await).is_empty(),
        "e0's old rank is de-indexed",
    );

    // EXTEND e1: expiry rises.
    let before = hex_quantity(&client.get_entity(key(1)).await["expiresAt"]);
    client
        .execute(vec![Operation::extend_expiry(key(1), 0, 5000)])
        .await;
    let after = hex_quantity(&client.get_entity(key(1)).await["expiresAt"]);
    assert!(
        after > before,
        "extend should raise expiresAt: {before} -> {after}"
    );

    // TRANSFER e2 to a new owner: it changes owner buckets.
    let bob = Address::from([0xBB; 20]);
    client
        .execute(vec![Operation::transfer_ownership(key(2), bob)])
        .await;
    assert_eq!(
        client.get_entity(key(2)).await["owner"].as_str(),
        Some(format!("{bob:#x}").as_str()),
    );
    assert_eq!(
        result_keys(
            &client
                .query(&format!("$owner = addr({bob:#x})"), 100, None)
                .await
        ),
        BTreeSet::from([ks(2)]),
        "e2 now in bob's bucket",
    );
    assert!(
        !result_keys(
            &client
                .query(&format!("$owner = addr({caller:#x})"), 100, None)
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

    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityNonce::new(0),
        0,
    ));
    let ks = format!("{key:#x}");

    client
        .execute(vec![create_op(
            1000,
            Bytes::from_static(b"v1"),
            attrs(10, "red"),
        )])
        .await;
    let created = hex_quantity(&client.get_entity(key).await["createdAt"]);

    client
        .execute(vec![{
            let mut muts = attrs(100, "red");
            muts.push(
                Attribute::from_value(
                    Ident32::system("$payload").unwrap(),
                    &AttributeValue::Bytes(b"v2".to_vec()),
                )
                .unwrap(),
            );
            Operation::patch(key, muts)
        }])
        .await;

    // The tip reflects the update.
    assert_eq!(client.get_entity(key).await["payload"], "0x7632"); // "v2"
    assert_eq!(
        result_keys(&client.query("rank = u256(100)", 100, None).await),
        BTreeSet::from([ks.clone()]),
    );
    assert!(result_keys(&client.query("rank = u256(10)", 100, None).await).is_empty());

    // As of the create block, both the entity and the query see the ORIGINAL state.
    assert_eq!(
        client.get_entity_at(key, created).await["payload"],
        "0x7631"
    ); // "v1"
    assert_eq!(
        result_keys(&client.query_at("rank = u256(10)", created).await),
        BTreeSet::from([ks.clone()]),
        "historical query sees the original rank",
    );
    assert!(
        result_keys(&client.query_at("rank = u256(100)", created).await).is_empty(),
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
        .execute(vec![create_op(1000, Bytes::from_static(b"v1"), vec![])])
        .await;
    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &owner_addr.into_array(),
        EntityNonce::new(0),
        0,
    ));
    assert_eq!(owner.get_entity(key).await["payload"], "0x7631"); // "v1"

    // The stranger cannot update or delete the owner's entity — both revert.
    assert!(
        !stranger
            .try_execute(vec![patch_payload(key, Bytes::from_static(b"hacked"))])
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
                create_op(1000, Bytes::from_static(b"batch"), vec![]),
                Operation::delete(phantom),
            ])
            .await,
        "a batch with a failing op must revert wholesale",
    );
    let would_be = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &owner_addr.into_array(),
        EntityNonce::new(1),
        0,
    ));
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

/// A lapsed BTL hides an entity from every read *without* an explicit `expire`
/// op: the index still carries it until someone prunes it, so the RPC layer
/// applies the expiry rule itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lapsed_btl_hides_an_entity_from_reads() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    // A TTL long enough to outlive the liveness check below, yet short enough
    // that the expiry block arrives in ~15s at 250ms blocks.
    client
        .execute(vec![create_op(60, Bytes::from_static(b"ttl"), vec![])])
        .await;
    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityNonce::new(0),
        0,
    ));
    let expires_at = hex_quantity(&client.get_entity(key).await["expiresAt"]);
    assert_eq!(
        client.entity_count(None, None).await,
        1,
        "live before expiry"
    );

    // Past the expiry block, with no `expire` op run against it.
    client
        .wait_for_block(expires_at, Duration::from_secs(30))
        .await;
    assert!(
        client.get_entity(key).await.is_null(),
        "past-BTL entity reads null"
    );
    assert!(
        !result_keys(&client.query("*", 100, None).await).contains(&format!("{key:#x}")),
        "past-BTL entity leaves queries",
    );
    assert_eq!(client.entity_count(None, None).await, 0, "and the count");

    // History still sees it: at its last live block it was live.
    assert!(
        !client.get_entity_at(key, expires_at - 1).await.is_null(),
        "a historical read before the expiry block still sees it",
    );
}

/// How many creates [`no_transaction_is_included_twice_over_a_live_node`] fires.
///
/// The bug it guards was reported at roughly 1 tx in 200, so this is a few
/// expected hits rather than a coin flip — the trigger is a race and a run that
/// happens to miss it proves nothing.
const DUPLICATE_PROBE_COUNT: u64 = 1_000;

/// A lifetime that cannot lapse within the probe's own block span, so the count
/// assertions below measure double-execution and not expiry.
const DUPLICATE_PROBE_LIFETIME: u64 = 100_000;

/// The gas the probe pins on every transaction, mirroring `ArkivClient`'s own
/// values so it skips `eth_estimateGas` — a round trip per tx would slow the
/// submission loop enough to stop keeping the pool full.
const DUPLICATE_PROBE_GAS: u64 = 8_000_000;
const DUPLICATE_PROBE_GAS_PRICE: u128 = 1_000_000_000;

/// The node's `--dev` block time, which is how long a full pool takes to drain.
const DUPLICATE_PROBE_BLOCK_TIME: Duration = Duration::from_millis(250);

/// **No transaction is executed twice.** A tx that has already been mined must
/// never be built into a second block.
///
/// This is a regression test for a reported `--dev` failure: at a 250ms block
/// time roughly 1 tx in 200 landed in two consecutive blocks — same hash, same
/// nonce — and its ops applied twice.
///
/// The race is reth's and is expected: `MiningMode::Interval` is a bare timer,
/// and mined transactions leave the pool asynchronously, on the canonical-state
/// stream. So a tick can snapshot `best_transactions` before the previous block's
/// eviction lands. reth absorbs this in its payload builder, which skips any
/// transaction the EVM rejects as `NonceTooLow` — a defence that only works if
/// the EVM *checks*. Arkiv's no-EVM executor did not, so the duplicate executed
/// cleanly and the resulting block was valid on every node.
///
/// Firing the whole batch without waiting is the point: the pool has to be
/// non-empty when the timer fires, or there is nothing to re-include.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_transaction_is_included_twice_over_a_live_node() {
    use std::collections::BTreeMap;

    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());

    let first_block = client.block_number().await;
    let nonce_start = client
        .provider()
        .get_transaction_count(caller)
        .await
        .expect("transaction count");

    // Submit with explicit sequential nonces and without awaiting receipts, so
    // the pool stays loaded across many miner ticks.
    //
    // The pool caps one sender at `max_account_slots` (16) pending transactions,
    // so this backs off a block on a full pool and retries the same nonce —
    // exactly what `arkiv-cli spam` does. That cap is a help here, not a
    // hindrance: it keeps the sender's queue saturated for the whole run.
    let mut pending = Vec::with_capacity(DUPLICATE_PROBE_COUNT as usize);
    for i in 0..DUPLICATE_PROBE_COUNT {
        let nonce = nonce_start + i;
        let sent = loop {
            let op = create_op(
                DUPLICATE_PROBE_LIFETIME,
                Bytes::from_static(b"dup-probe"),
                vec![],
            );
            match registry
                .execute(vec![op])
                .nonce(nonce)
                .gas(DUPLICATE_PROBE_GAS)
                .gas_price(DUPLICATE_PROBE_GAS_PRICE)
                .send()
                .await
            {
                Ok(sent) => break sent,
                Err(e) if e.to_string().contains("txpool is full") => {
                    tokio::time::sleep(DUPLICATE_PROBE_BLOCK_TIME).await;
                }
                Err(e) => panic!("submitting probe tx {i} at nonce {nonce}: {e}"),
            }
        };
        pending.push(sent);
    }

    // Collect every receipt, and with them the block span to scan.
    let mut submitted = BTreeSet::new();
    let mut last_block = first_block;
    for (i, sent) in pending.into_iter().enumerate() {
        let hash = *sent.tx_hash();
        let receipt = sent
            .with_timeout(Some(Duration::from_secs(120)))
            .get_receipt()
            .await
            .unwrap_or_else(|e| panic!("no receipt for probe tx {i} ({hash}): {e}"));
        assert!(receipt.status(), "probe tx {i} ({hash}) reverted");
        last_block = last_block.max(receipt.block_number.expect("mined receipt has a block"));
        submitted.insert(hash);
    }
    assert_eq!(
        submitted.len() as u64,
        DUPLICATE_PROBE_COUNT,
        "each probe tx should have its own hash",
    );

    // Walk the block bodies. Receipts cannot answer this: a second inclusion has
    // its own receipt that nothing would think to ask for.
    let mut inclusions: BTreeMap<B256, Vec<u64>> = BTreeMap::new();
    for number in first_block..=last_block {
        let Some(hashes) = client.block_tx_hashes(number).await else {
            panic!("block {number} missing while scanning [{first_block}, {last_block}]");
        };
        for hash in hashes {
            inclusions.entry(hash).or_default().push(number);
        }
    }

    let duplicated: Vec<_> = inclusions
        .iter()
        .filter(|(_, blocks)| blocks.len() > 1)
        .map(|(hash, blocks)| format!("{hash} in blocks {blocks:?}"))
        .collect();
    assert!(
        duplicated.is_empty(),
        "{} of {DUPLICATE_PROBE_COUNT} transactions were included more than once \
         (scanned blocks {first_block}..={last_block}): {}",
        duplicated.len(),
        duplicated.join(", "),
    );

    // The state-level cross-check: a create that ran twice mints a *second*
    // entity under a different key, because the minting nonce advanced in
    // between. Both of these over-count if any batch was applied twice, and they
    // catch a double execution even if the block scan somehow missed it.
    assert_eq!(
        client.entity_count(None, None).await,
        DUPLICATE_PROBE_COUNT,
        "one entity per probe tx",
    );
    assert_eq!(
        registry.entityNonce(caller).call().await.unwrap(),
        DUPLICATE_PROBE_COUNT,
        "one minting-nonce step per probe tx",
    );
}

/// **A pooled deployment transaction must not stall block production.**
///
/// The stock pool accepts contract-creation transactions — nothing filters them
/// at ingress — and never evicts an unmined one, so the executor's rejection is
/// what the payload builder sees on *every* build. How it rejects is
/// load-bearing: a typed invalid-tx error is skipped, while anything else
/// (`EVMError::Custom`) aborts the whole build as fatal. With the same tx
/// re-offered on every rebuild, one deploy tx submitted to a dev node would
/// then halt the chain permanently: no block is produced again and every later
/// transaction times out.
///
/// The probe: pool a deploy tx, then prove the chain still works. A normal
/// entity create from a *different* sender must still mine — the deploy
/// legitimately queues its own sender's later nonces behind it, so a same-sender
/// probe would prove nothing — and the deploy itself must never mine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pooled_create_transaction_does_not_stall_block_production() {
    let (node, client, caller) = spawn_dev(DEV_KEY_0).await;

    // A deployment tx: `to: None`, one STOP byte of initcode. Every gas field is
    // pinned so no filler round-trips through eth_estimateGas — estimating a
    // create is itself rejected by the executor.
    let nonce = client
        .provider()
        .get_transaction_count(caller)
        .await
        .expect("transaction count");
    let deploy = TransactionRequest::default()
        .with_deploy_code(Bytes::from_static(&[0x00]))
        .with_nonce(nonce)
        .with_gas_limit(1_000_000)
        .with_gas_price(1_000_000_000)
        .with_chain_id(DEV_CHAIN_ID);
    // Ingress must ACCEPT it — that the pool lets creates through is the premise
    // of the whole scenario.
    let deploy_hash = *client
        .provider()
        .send_transaction(deploy)
        .await
        .expect("the pool accepts a deployment transaction at ingress")
        .tx_hash();

    // With the deploy sitting in the pool, an independent sender's create must
    // still be built and mined.
    let bystander_signer: PrivateKeySigner = DEV_KEY_1.parse().unwrap();
    let bystander_addr = bystander_signer.address();
    let bystander = connect(&node.http_url(), bystander_signer);
    bystander
        .execute(vec![create_op(1000, Bytes::from_static(b"alive"), vec![])])
        .await;

    let key = B256::from(derive_entity_key(
        DEV_CHAIN_ID,
        &bystander_addr.into_array(),
        EntityNonce::new(0),
        0,
    ));
    assert_eq!(
        bystander.get_entity(key).await["payload"],
        "0x616c697665", // "alive"
        "the bystander's create must land while the deploy is pooled",
    );

    // And the deploy was skipped, not mined: no receipt, though the chain has
    // demonstrably moved past it.
    assert!(
        client
            .provider()
            .get_transaction_receipt(deploy_hash)
            .await
            .expect("eth_getTransactionReceipt")
            .is_none(),
        "a contract deployment must never be mined",
    );
}
