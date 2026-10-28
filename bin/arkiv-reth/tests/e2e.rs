//! End-to-end tests: spawn the real `arkiv-reth` binary and drive it like a
//! client, through the [`arkiv_harness`] node + RPC-client layer.
//!
//! Black-box: it exercises the wired executor (decode → apply → commit), the
//! minting nonce, the `arkiv_*` read path, ownership/atomicity rules, expiry, and
//! historical reads — all against real reth state over JSON-RPC.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use arkiv_bindings::{
    Attribute, AttributeType, AttributeValue, ENTITY_MAX_ATTRIBUTES, IEntityRegistry, Ident32,
    MAX_PURGE_KEYS, Operation, protocol::purgeExpiredCall,
};
use arkiv_harness::{
    ARKIV_ADDRESS, ArkivClient, DEV_CHAIN_ID, DEV_KEY_0, DEV_KEY_1, EntityCreationNonce,
    NodeBuilder, connect, derive_entity_address, hex_quantity, result_keys,
};
use arkiv_interfaces::constants::{MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};

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

async fn physical_entity_count_at(
    client: &ArkivClient<impl Provider>,
    keys: &[B256],
    block: u64,
) -> usize {
    let mut count = 0;
    for key in keys {
        count += usize::from(client.debug_entity_exists_at(*key, block).await);
    }
    count
}

async fn wait_for_first_purge(
    client: &ArkivClient<impl Provider>,
    keys: &[B256],
    first_block: u64,
) -> (u64, usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut block = first_block;
    loop {
        let tip = client.block_number().await;
        while block <= tip {
            let remaining = physical_entity_count_at(client, keys, block).await;
            if remaining < keys.len() {
                return (block, remaining);
            }
            block += 1;
        }
        assert!(
            Instant::now() < deadline,
            "no physical purge occurred within 30 seconds"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_all_purged(client: &ArkivClient<impl Provider>, keys: &[B256]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut remaining = 0;
        for key in keys {
            remaining += usize::from(client.debug_entity_exists(*key).await);
        }
        if remaining == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "physical pruning did not finish within 30 seconds"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A create carrying `payload` under `text/plain`, with a purely relative
/// lifetime. Both are ordinary **unindexed** user attributes: stored and
/// returned, never queryable.
fn create_op(min_lifetime: u64, payload: Bytes, mut attrs: Vec<Attribute>) -> Operation {
    attrs.push(
        Attribute::from_value(
            Ident32::encode("contentType").unwrap(),
            &AttributeValue::Str("text/plain".into()),
            false,
        )
        .unwrap(),
    );
    attrs.push(payload_attr(payload));
    Operation::create(0, 0, min_lifetime, 0, attrs)
}

/// The unindexed `bytes` attribute named `payload`.
fn payload_attr(payload: Bytes) -> Attribute {
    Attribute::from_value(
        Ident32::encode("payload").unwrap(),
        &AttributeValue::Bytes(payload.to_vec()),
        false,
    )
    .unwrap()
}

/// The JSON value of the attribute `name` in an entity read back over RPC, or
/// null when the entity does not carry it.
fn attr_value<'a>(entity: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    entity["attributes"]
        .as_array()
        .and_then(|attrs| attrs.iter().find(|a| a["name"] == name))
        .map_or(&serde_json::Value::Null, |a| &a["value"])
}

/// A patch that replaces just the payload.
fn patch_payload(key: B256, payload: Bytes) -> Operation {
    Operation::patch(key, vec![payload_attr(payload)])
}

/// A `rank` (uint) + `team` (string) attribute pair — the two axes the query
/// matrix filters on.
fn attrs(rank: u64, team: &str) -> Vec<Attribute> {
    let attr = |name: &str, value: AttributeValue| {
        Attribute::from_value(Ident32::encode(name).unwrap(), &value, true).unwrap()
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
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
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
    assert_eq!(attr_value(&entity, "contentType"), "text/plain");
    assert_eq!(attr_value(&entity, "payload"), "0x68656c6c6f"); // "hello"
    let created = hex_quantity(&entity["createdAt"]);
    assert_eq!(hex_quantity(&entity["expiresAt"]), created + 100); // btl resolved
    // Attributes are typed, in the same shape `arkiv_query` answers with.
    assert_eq!(
        entity["attributes"],
        serde_json::json!([
            { "name": "contentType", "type": "str", "value": "text/plain" },
            { "name": "payload", "type": "bytes", "value": "0x68656c6c6f" },
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
                "creationFlags": true, "attributes": true,
            }}),
        )
        .await
        .expect("query by key");
    assert_eq!(queried["data"][0], entity, "getEntity and query must agree");

    // 3b) The schema reports each attribute's indexed flag, and the
    //     `attributeTypeId` view answers with the wire byte, bit 7 included.
    let schema = client
        .query_raw(
            &format!("$key = key({key:#x})"),
            serde_json::json!({ "select": { "attributeSchema": true } }),
        )
        .await
        .expect("schema by key");
    assert_eq!(
        schema["data"][0]["attributeSchema"],
        serde_json::json!([
            { "name": "contentType", "type": "str", "indexed": false },
            { "name": "payload", "type": "bytes", "indexed": false },
            { "name": "rank", "type": "u256", "indexed": true },
            { "name": "team", "type": "str", "indexed": true },
        ])
    );
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());
    for (name, type_id) in [("payload", 0x07), ("rank", 0x84), ("missing", 0x00)] {
        let got = registry
            .attributeTypeId(key, Ident32::encode(name).unwrap().into_word())
            .call()
            .await
            .unwrap();
        assert_eq!(got, type_id, "{name}");
    }

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
    use alloy_sol_types::SolError;

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
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
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

    // A slash in an attribute name causes Ident32InvalidByte.
    let mut name = [0u8; 32];
    name[..14].copy_from_slice(b"test/attribute");
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
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
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

    // Delete: reth runs every estimate probe on one EVM. If a probe's delete
    // stuck, the next would revert EntityNotFound and the estimate would climb
    // to the block gas cap.
    let delete = registry.execute(vec![Operation::delete(key)]);
    let estimate = delete.estimate_gas().await.expect("delete estimates");
    assert!(estimate < 1_000_000, "delete estimated at {estimate}");
    assert!(
        !client.get_entity(key).await.is_null(),
        "estimating a delete must not delete"
    );
    let receipt = delete
        .send()
        .await
        .expect("delete with estimated gas accepted by the pool")
        .get_receipt()
        .await
        .expect("delete receipt");
    assert!(receipt.status(), "delete must succeed");
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
                B256::from(derive_entity_address(
                    DEV_CHAIN_ID,
                    &caller.into_array(),
                    EntityCreationNonce::new(i),
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
    // Every type ranges, a `str` in raw byte order: blue < green < re < red.
    assert_eq!(
        result_keys(&client.query("team > str('blue')", 100, None).await),
        expect(&[0, 1, 4])
    );
    assert_eq!(
        result_keys(&client.query("team < str('re')", 100, None).await),
        expect(&[2, 3, 4])
    );
    // A `key` ranges as unsigned bytes, which is the order of its fixed-width
    // lowercase hex.
    let mut by_key = k.clone();
    by_key.sort();
    assert_eq!(
        result_keys(
            &client
                .query(&format!("$key >= key({})", by_key[2]), 100, None)
                .await
        ),
        by_key[2..].iter().cloned().collect::<BTreeSet<_>>()
    );
    // BETWEEN includes both bounds, and its AND is its own.
    assert_eq!(
        result_keys(
            &client
                .query("rank BETWEEN u256(20) AND u256(40)", 100, None)
                .await
        ),
        expect(&[1, 2, 3])
    );
    assert_eq!(
        result_keys(
            &client
                .query(
                    "team BETWEEN str('blue') AND str('green') AND rank > u256(30)",
                    100,
                    None
                )
                .await
        ),
        expect(&[3, 4])
    );
    assert!(
        result_keys(
            &client
                .query("rank BETWEEN u256(40) AND u256(20)", 100, None)
                .await
        )
        .is_empty(),
        "an inverted BETWEEN matches nothing"
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

    // Presence, without comparing a value: `exists` spans every type the
    // attribute is carried under, `typeof` narrows it to one. All five
    // entities carry both attributes.
    assert_eq!(
        result_keys(&client.query("exists(rank)", 100, None).await),
        expect(&[0, 1, 2, 3, 4])
    );
    assert_eq!(
        result_keys(&client.query("typeof(rank) = u256", 100, None).await),
        expect(&[0, 1, 2, 3, 4])
    );
    assert_eq!(
        result_keys(&client.query("typeof(team) = str", 100, None).await),
        expect(&[0, 1, 2, 3, 4])
    );
    assert!(
        result_keys(&client.query("exists(absent)", 100, None).await).is_empty(),
        "no entity carries an attribute nothing wrote",
    );
    // `typeof` is as type-exact as a value predicate: `rank` is a u256.
    assert!(
        result_keys(&client.query("typeof(rank) = i32", 100, None).await).is_empty(),
        "typeof must not match an attribute of another type",
    );
    assert_eq!(
        result_keys(
            &client
                .query("exists(rank) AND team = str('blue')", 100, None)
                .await
        ),
        expect(&[2, 3]),
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
    // Every entity carries `contentType = 'text/plain'`, but unindexed: stored
    // and returned, never matched.
    assert_eq!(
        result_keys(
            &client
                .query("contentType = str('text/plain')", 100, None)
                .await
        ),
        expect(&[]),
    );
    // Block heights are u64 and must say so — an untagged number means i32.
    assert_eq!(
        result_keys(&client.query("$expiresAt >= u64(1)", 100, None).await),
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
        ("$expiresAt >= 1", "-32002"),                        // system u64 needs its tag
        ("$createdAt >= u64(1)", "-32002"),                   // returned, never queried
        ("rank != u256(30)", "-32002"),                       // != is not in the language
        ("rank STARTSWITH u256(1)", "-32002"),                // a prefix needs a str
        ("rank BETWEEN u256(1) AND i32(5)", "-32002"),        // bounds of two types
        ("between = true", "-32001"),                         // a reserved word
        ("exists($owner)", "-32002"),                         // presence takes a user attribute
        ("typeof(rank) = bytes", "-32002"), // bytes is never indexed, so not a tag
        ("rank = i32(2147483648)", "-32003"), // literal out of range
        (
            "who = addr(0x5aAeb6053f3E94C9b9A09f33669435E7Ef1BeAed)",
            "-32003",
        ), // bad EIP-55 checksum
        (&"(".repeat(200), "-32004"),       // nested too deeply
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

    // `limit` is bounded rather than silently trimmed, in both directions: 101
    // is one past the node maximum.
    for bad_limit in [serde_json::json!(0), serde_json::json!(101)] {
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

    // `select.count` is the size of the whole match set. Unselected, it is
    // absent; selected alone, it is the entire answer: no entities, no cursor.
    assert!(first.get("count").is_none(), "count only when selected");
    let count_only = client
        .query_raw(
            "rank >= u256(20)",
            serde_json::json!({ "select": { "count": true } }),
        )
        .await
        .expect("count alone");
    assert_eq!(count_only["count"], "0x4", "four ranks of 20 and up");
    assert_eq!(count_only["data"], serde_json::json!([]));
    assert!(
        count_only.get("cursor").is_none(),
        "no page, so no next one"
    );

    // Beside a field, it comes with every page, unchanged by the paging.
    let counted = serde_json::json!({ "select": { "key": true, "count": true }, "limit": 3 });
    let counted_page = client
        .query_raw("rank >= u256(20)", counted.clone())
        .await
        .expect("first counted page");
    assert_eq!(counted_page["count"], "0x4");
    assert_eq!(result_keys(&counted_page).len(), 3);
    let mut rest = counted;
    rest["cursor"] = counted_page["cursor"].clone();
    let counted_rest = client
        .query_raw("rank >= u256(20)", rest)
        .await
        .expect("second counted page");
    assert_eq!(counted_rest["count"], "0x4", "the match set, not the page");
    assert_eq!(result_keys(&counted_rest).len(), 1);
    assert!(counted_rest.get("cursor").is_none());
}

/// Every mutating op — update, extend, transfer, delete — driven over a live node
/// and observed through `arkiv_getEntity`, `arkiv_query`, and `arkiv_getEntityCount`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_path_ops_over_a_live_node() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;

    let key = |i: u64| {
        B256::from(derive_entity_address(
            DEV_CHAIN_ID,
            &caller.into_array(),
            EntityCreationNonce::new(i),
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
            muts.push(payload_attr(Bytes::from_static(b"v2")));
            Operation::patch(key(0), muts)
        }])
        .await;
    let e0 = client.get_entity(key(0)).await;
    assert_eq!(attr_value(&e0, "payload"), "0x7632"); // "v2"
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

    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
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
            muts.push(payload_attr(Bytes::from_static(b"v2")));
            Operation::patch(key, muts)
        }])
        .await;

    // The tip reflects the update.
    assert_eq!(
        attr_value(&client.get_entity(key).await, "payload"),
        "0x7632"
    ); // "v2"
    assert_eq!(
        result_keys(&client.query("rank = u256(100)", 100, None).await),
        BTreeSet::from([ks.clone()]),
    );
    assert!(result_keys(&client.query("rank = u256(10)", 100, None).await).is_empty());

    // As of the create block, both the entity and the query see the ORIGINAL state.
    assert_eq!(
        attr_value(&client.get_entity_at(key, created).await, "payload"),
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
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &owner_addr.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    assert_eq!(
        attr_value(&owner.get_entity(key).await, "payload"),
        "0x7631"
    ); // "v1"

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
        attr_value(&owner.get_entity(key).await, "payload"),
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
    let would_be = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &owner_addr.into_array(),
        EntityCreationNonce::new(1),
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
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
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

/// The producer's protocol transaction physically removes a short-lived entity;
/// this uses the debug read because the public read hides expiry even without a purge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_entity_is_physically_purged() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    client
        .execute(vec![create_op(8, Bytes::from_static(b"purge-me"), vec![])])
        .await;
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    let expires_at = hex_quantity(&client.get_entity(key).await["expiresAt"]);
    assert!(
        client.debug_entity_exists(key).await,
        "record exists before expiry"
    );

    // A successful simulation must not remove the canonical schedule entry.
    let calldata = IEntityRegistry::executeCall {
        ops: vec![Operation::delete(key)],
    }
    .abi_encode();
    client
        .provider()
        .raw_request::<_, Bytes>(
            "eth_call".into(),
            (
                serde_json::json!({
                    "from": caller,
                    "to": ARKIV_ADDRESS,
                    "data": format!("0x{}", alloy_primitives::hex::encode(calldata)),
                }),
                "latest",
            ),
        )
        .await
        .expect("simulated delete succeeds");
    assert!(
        client.debug_entity_exists(key).await,
        "simulation does not delete canonical state"
    );

    let (purged_at, remaining) = wait_for_first_purge(&client, &[key], expires_at).await;
    assert_eq!(remaining, 0, "the entity is physically purged");
    assert!(
        client.get_entity(key).await.is_null(),
        "expired entity is logically absent"
    );
    assert!(
        !client.debug_entity_exists_at(key, purged_at).await,
        "expired entity record was physically purged"
    );
    assert!(
        client.debug_entity_exists_at(key, purged_at - 1).await,
        "record existed before the purge block"
    );
}

/// The count cap drains one common-expiry cohort over successive blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_respects_max_keys_per_block() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let entity_count = u64::try_from(MAX_PURGE_KEYS).unwrap() + 1;
    client
        .execute(
            (0..entity_count)
                .map(|_| create_op(8, Bytes::new(), vec![]))
                .collect(),
        )
        .await;
    let keys: Vec<_> = (0..entity_count)
        .map(|nonce| {
            B256::from(derive_entity_address(
                DEV_CHAIN_ID,
                &caller.into_array(),
                EntityCreationNonce::new(nonce),
                0,
            ))
        })
        .collect();
    let expires_at = hex_quantity(&client.get_entity(keys[0]).await["expiresAt"]);

    let (_, remaining) = wait_for_first_purge(&client, &keys, expires_at).await;
    assert_eq!(remaining, 1, "the per-block key limit is enforced");
    wait_for_all_purged(&client, &keys).await;
}

/// Patch-expanded entities obey the strict purge transaction gas limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_respects_strict_gas_limit_after_attribute_patches() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let keys: Vec<_> = (0..6)
        .map(|nonce| {
            B256::from(derive_entity_address(
                DEV_CHAIN_ID,
                &caller.into_array(),
                EntityCreationNonce::new(nonce),
                0,
            ))
        })
        .collect();

    // Bare creates: the patches below use the whole attribute budget.
    let mut ops: Vec<_> = (0..6)
        .map(|_| Operation::create(0, 0, 8, 0, vec![]))
        .collect();
    // Each patch grows the stored entity to the per-operation maximum. Six
    // purges would cost 1.02m gas, so only five fit in the 1m transaction.
    for (entity, key) in keys.iter().enumerate() {
        let attributes = (0..ENTITY_MAX_ATTRIBUTES)
            .map(|offset| {
                let name = format!("a{entity}_{offset:02}");
                Attribute::from_value(
                    Ident32::encode(&name).unwrap(),
                    &AttributeValue::u256_from_u64(offset as u64),
                    true,
                )
                .unwrap()
            })
            .collect();
        ops.push(Operation::patch(*key, attributes));
    }
    client.execute(ops).await;
    let expires_at = hex_quantity(&client.get_entity(keys[0]).await["expiresAt"]);
    for key in &keys {
        assert_eq!(
            client.get_entity(*key).await["attributes"]
                .as_array()
                .unwrap()
                .len(),
            ENTITY_MAX_ATTRIBUTES
        );
    }

    let (_, remaining) = wait_for_first_purge(&client, &keys, expires_at).await;
    assert_eq!(
        remaining, 1,
        "the sixth purge does not exceed the gas limit"
    );
    wait_for_all_purged(&client, &keys).await;
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

    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &bystander_addr.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    assert_eq!(
        attr_value(&bystander.get_entity(key).await, "payload"),
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

/// A user can submit `purgeExpired` to the stock pool, but the executor must
/// reject it as a skippable invalid transaction instead of aborting payloads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pooled_user_purge_does_not_stall_block_production() {
    let (node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let nonce = client
        .provider()
        .get_transaction_count(caller)
        .await
        .expect("transaction count");
    let purge = TransactionRequest::default()
        .with_to(ARKIV_ADDRESS)
        // Selector only: deliberately malformed ABI for purgeExpired(bytes32[]).
        .with_input(Bytes::copy_from_slice(&purgeExpiredCall::SELECTOR))
        .with_nonce(nonce)
        .with_gas_limit(1_000_000)
        .with_gas_price(1_000_000_000)
        .with_chain_id(DEV_CHAIN_ID);
    let purge_hash = *client
        .provider()
        .send_transaction(purge)
        .await
        .expect("the pool accepts purge calldata at ingress")
        .tx_hash();

    let bystander_signer: PrivateKeySigner = DEV_KEY_1.parse().unwrap();
    let bystander_addr = bystander_signer.address();
    let bystander = connect(&node.http_url(), bystander_signer);
    bystander
        .execute(vec![create_op(
            1000,
            Bytes::from_static(b"still-building"),
            vec![],
        )])
        .await;
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &bystander_addr.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    assert_eq!(
        attr_value(&bystander.get_entity(key).await, "payload"),
        "0x7374696c6c2d6275696c64696e67"
    );
    assert!(
        client
            .provider()
            .get_transaction_receipt(purge_hash)
            .await
            .expect("eth_getTransactionReceipt")
            .is_none(),
        "the user purge must be skipped, not mined"
    );
}

/// Keys a caller can name where GolemDB holds a record that is not an entity:
/// an account's, the fee schedule's and GolemDB's own. An operation there
/// reverts and a view call is mined; a store fault instead fails every block
/// built while it is pooled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_that_hold_no_entity_find_none() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .golemdb_store()
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    let transactions = || async {
        client
            .provider()
            .get_transaction_count(caller)
            .await
            .expect("transaction count")
    };
    let nonce = transactions().await;

    let account = B256::left_padding_from(caller.as_slice());
    // Written at genesis, beside the entities.
    let schedule = B256::from(arkiv_interfaces::entity_records::FEE_SCHEDULE_KEY);
    // `#recordKeys` holds a cell for every record in the store.
    let golemdb = [b"#params".as_slice(), b"#recordKeys"].map(B256::right_padding_from);
    let keys = [account, schedule, golemdb[0], golemdb[1]];

    let mut ops = Vec::new();
    for key in [account, schedule] {
        ops.extend([
            patch_payload(key, Bytes::from_static(b"x")),
            Operation::extend_expiry(key, 0, 5000),
            Operation::transfer_ownership(key, Address::repeat_byte(0xbb)),
        ]);
    }
    ops.extend(keys.map(Operation::delete));
    for op in &ops {
        assert!(
            !client.try_execute(vec![op.clone()]).await,
            "an operation on a key that holds no entity must revert"
        );
    }

    for key in keys {
        assert!(client.get_entity(key).await.is_null());
        assert!(!client.debug_entity_exists(key).await);

        let view = TransactionRequest::default()
            .with_to(ARKIV_ADDRESS)
            .with_input(IEntityRegistry::customAttributeNamesCall { entityKey: key }.abi_encode())
            .with_nonce(transactions().await)
            .with_gas_limit(1_000_000)
            .with_gas_price(1_000_000_000)
            .with_chain_id(DEV_CHAIN_ID);
        client
            .provider()
            .send_transaction(view)
            .await
            .expect("the pool accepts a view call")
            .with_timeout(Some(Duration::from_secs(30)))
            .get_receipt()
            .await
            .expect("a view call sent as a transaction is mined");
    }

    // Still an account: its nonce counts every transaction.
    assert_eq!(
        transactions().await,
        nonce + (ops.len() + keys.len()) as u64
    );
}

/// The base fee of every block up to `until`, as the RPC reports it.
async fn base_fees(client: &ArkivClient<impl Provider>, until: u64) -> Vec<u64> {
    let mut fees = Vec::new();
    for number in 0..=until {
        let block = client
            .provider()
            .get_block_by_number(number.into())
            .await
            .expect("eth_getBlockByNumber")
            .unwrap_or_else(|| panic!("block {number} exists"));
        fees.push(block.header.base_fee_per_gas.expect("post-London header"));
    }
    fees
}

/// The Arkiv base-fee rule on `--dev`: an idle chain holds reth's 1 gwei dev-genesis
/// base fee instead of decaying 12.5% per empty block toward the 7 wei protocol
/// minimum. Stock reth would report 1 gwei, 875 M, 765 M, ... here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_dev_chain_holds_the_genesis_base_fee() {
    let (_node, client, _caller) = spawn_dev(DEV_KEY_0).await;
    client.wait_for_block(6, READY).await;

    let fees = base_fees(&client, 6).await;
    assert_eq!(
        fees,
        vec![1_000_000_000; 7],
        "every empty block must carry the genesis base fee",
    );

    // And a transaction priced at exactly the floor is what the SDK sends by
    // default (1 gwei): the pool accepts it and it is mined.
    client
        .execute(vec![create_op(1000, Bytes::from_static(b"floor"), vec![])])
        .await;
}

/// The chain ID of [`genesis_json`].
const FILE_CHAIN_ID: u64 = 424242;

/// A genesis file's contents: every fork from block 0, and [`DEV_KEY_0`] funded.
fn genesis_json() -> serde_json::Value {
    let funded = DEV_KEY_0.parse::<PrivateKeySigner>().unwrap().address();
    serde_json::json!({
        "config": {
            "chainId": FILE_CHAIN_ID,
            "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
            "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "mergeNetsplitBlock": 0, "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
        },
        "gasLimit": "0x3938700",
        "difficulty": "0x0",
        "alloc": { format!("{funded:#x}"): { "balance": "0xd3c21bcecceda1000000" } }
    })
}

/// `config.arkiv` giving GolemDB `hash_function` and Arkiv's limits.
fn arkiv_section(hash_function: &str) -> serde_json::Value {
    serde_json::json!({
        "golemdb": {
            "hash_function": hash_function,
            "cell_limits": {
                "max_cell_name_len": MAX_ATTRIBUTE_NAME_BYTES,
                "max_str_len": MAX_STR_BYTES,
                "max_bytes_len": MAX_BYTES_VALUE_LEN
            },
            "record_limits": {
                "max_cells": MAX_RECORD_CELLS,
                "max_indexed_cells": MAX_RECORD_INDEXED_CELLS
            }
        },
        "tokenomics": {
            "capacity_rate": 14,
            "cell_byte_units": 1,
            "index_byte_units": 1,
            "c_cell": 64,
            "c_idx": 64,
            "max_lifetime": 7_884_000
        }
    })
}

/// Write `genesis` to a file of this test's own and return its path.
fn genesis_file(genesis: &serde_json::Value) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "arkiv-e2e-genesis-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, genesis.to_string()).expect("write genesis");
    path
}

/// Wait for `node` to stop at startup, and return its log.
async fn refusal(node: &mut arkiv_harness::Node) -> String {
    let status = node.wait_exit(READY).await;
    assert!(!status.success(), "the node exits with an error ({status})");
    std::fs::read_to_string(node.log_path()).expect("read the node log")
}

/// The chain's base fee never drops below the genesis file's `baseFeePerGas`.
/// Here that is 10 wei, so the first 7 blocks all have a base fee of 10.
/// Without this rule, empty blocks would lower it to 7.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn base_fee_floor_follows_the_genesis_file() {
    let mut genesis = genesis_json();
    genesis["config"]["arkiv"] = arkiv_section("keccak-256");
    genesis["baseFeePerGas"] = "0xa".into();
    let genesis_path = genesis_file(&genesis);

    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis_path)
        .spawn();
    let client = arkiv_harness::connect_reader(&node.http_url());
    node.wait_ready(&client, READY).await;
    assert_eq!(client.chain_id().await, FILE_CHAIN_ID);
    client.wait_for_block(6, READY).await;

    let fees = base_fees(&client, 6).await;
    let _ = std::fs::remove_file(&genesis_path);
    assert_eq!(fees, vec![10; 7], "the floor is the genesis baseFeePerGas");
}

/// The lifetime bound is the genesis file's: a chain whose fee schedule allows
/// 1,000 blocks takes a create for exactly that, and refuses one block more
/// with `ExpiryBeyondMaxLifetime`, naming the configured bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lifetime_bound_follows_the_genesis_file() {
    use alloy_sol_types::SolError;

    let mut genesis = genesis_json();
    genesis["config"]["arkiv"] = arkiv_section("keccak-256");
    genesis["config"]["arkiv"]["tokenomics"]["max_lifetime"] = 1_000.into();
    let genesis_path = genesis_file(&genesis);
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis_path)
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;

    let receipt = client
        .execute(vec![create_op(
            1_000,
            Bytes::from_static(b"at the bound"),
            vec![],
        )])
        .await;
    assert!(receipt.status(), "a create for exactly max_lifetime lands");

    let calldata = IEntityRegistry::executeCall {
        ops: vec![create_op(1_001, Bytes::from_static(b"beyond"), vec![])],
    }
    .abi_encode();
    let err = client
        .provider()
        .raw_request::<_, Bytes>(
            "eth_call".into(),
            (
                serde_json::json!({
                    "from": caller,
                    "to": ARKIV_ADDRESS,
                    "data": format!("0x{}", alloy_primitives::hex::encode(&calldata)),
                }),
                "latest",
            ),
        )
        .await
        .expect_err("one block more must revert")
        .to_string();
    let _ = std::fs::remove_file(&genesis_path);
    assert!(
        err.contains(&alloy_primitives::hex::encode(
            IEntityRegistry::ExpiryBeyondMaxLifetime::SELECTOR
        )),
        "expected ExpiryBeyondMaxLifetime revert data, got: {err}"
    );
    assert!(
        err.contains(&alloy_primitives::hex::encode(B256::from(U256::from(
            1_000
        )))),
        "the revert names the genesis file's bound: {err}"
    );
}

/// GolemDB's settings follow the genesis file: a chain on Blake3 runs and takes
/// writes. Its store then refuses the same file on Keccak-256, and opens again
/// for the right file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golemdb_settings_follow_the_genesis_file() {
    let mut genesis = genesis_json();
    genesis["config"]["arkiv"] = arkiv_section("blake3");
    let genesis_path = genesis_file(&genesis);
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis_path)
        .golemdb_store()
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    let log = std::fs::read_to_string(node.log_path()).unwrap();
    assert!(
        log.contains("Blake3"),
        "the node logs the settings in use: {log}"
    );

    let receipt = client
        .execute(vec![create_op(1000, Bytes::from_static(b"blake3"), vec![])])
        .await;
    // Past reth's persistence threshold of 50 blocks, as the restart tests
    // wait, so a restart resumes the chain. The store has the write by then.
    let mined = receipt.block_number.expect("mined");
    client.wait_for_block(mined + 60, READY).await;
    let key = B256::from(derive_entity_address(
        FILE_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    let entity = client.get_entity(key).await;
    assert_eq!(attr_value(&entity, "payload"), "0x626c616b6533"); // "blake3"
    node.terminate();

    let mut keccak = genesis.clone();
    keccak["config"]["arkiv"] = arkiv_section("keccak-256");
    std::fs::write(&genesis_path, keccak.to_string()).unwrap();
    node.restart();
    let log = refusal(&mut node).await;
    assert!(
        log.contains("it was created with other GolemDB settings"),
        "{log}"
    );

    std::fs::write(&genesis_path, genesis.to_string()).unwrap();
    node.restart();
    node.wait_ready(&client, READY).await;
    let _ = std::fs::remove_file(&genesis_path);
    assert_eq!(client.get_entity(key).await, entity, "the entity survives");
}

/// A GolemDB chain whose genesis file lacks a usable `config.arkiv.golemdb`
/// stops at startup, with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_arkiv_section_stops_the_node() {
    let mut top_level = genesis_json();
    top_level["arkiv"] = arkiv_section("keccak-256");
    let mut misnamed = genesis_json();
    misnamed["config"]["arkiv"] = serde_json::json!({ "golemdb": { "hash_fn": "keccak-256" } });
    let mut limits = genesis_json();
    limits["config"]["arkiv"] = arkiv_section("keccak-256");
    limits["config"]["arkiv"]["golemdb"]["cell_limits"]["max_str_len"] = (MAX_STR_BYTES / 2).into();
    let mut segments = genesis_json();
    segments["config"]["arkiv"] = arkiv_section("keccak-256");
    segments["config"]["arkiv"]["golemdb"]["immutable_data"] =
        serde_json::json!({ "shard_span": 1, "segments": [] });
    for (genesis, expected) in [
        (genesis_json(), "no config.arkiv.golemdb"),
        (top_level, "no config.arkiv.golemdb"),
        (misnamed, "unknown field `hash_fn`"),
        (limits, "must be Arkiv's limits"),
        (segments, "the node declares its own segments"),
    ] {
        let genesis_path = genesis_file(&genesis);
        let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
            .chain(&genesis_path)
            .golemdb_store()
            .spawn();
        let log = refusal(&mut node).await;
        let _ = std::fs::remove_file(&genesis_path);
        assert!(log.contains(expected), "{expected}: {log}");
    }
}

/// A chain without a fee schedule stops at startup on any store, not only on
/// GolemDB, which needs `config.arkiv` for its own settings first: here the
/// node runs in memory and only the schedule is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_without_a_fee_schedule_stops_the_node() {
    let genesis_path = genesis_file(&genesis_json());
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis_path)
        .spawn();
    let log = refusal(&mut node).await;
    let _ = std::fs::remove_file(&genesis_path);
    assert!(
        log.contains("no config.arkiv.tokenomics"),
        "the node names the missing fee schedule: {log}"
    );
}

/// On GolemDB, the store's genesis must have block 0's `stateRoot`. A store
/// with this chain's settings and genesis tag, but other accounts, has another
/// root and is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_golemdb_store_with_another_genesis_root_is_refused() {
    use arkiv_reth_statemanager::{GenesisAccount, HostStore, seed_genesis, segments};
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../genesis/example.json");
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(example)
        .golemdb_store()
        .spawn();
    let client = arkiv_harness::connect_reader(&node.http_url());
    node.wait_ready(&client, READY).await;
    let genesis_hash = client
        .provider()
        .get_block_by_number(0.into())
        .await
        .expect("eth_getBlockByNumber")
        .expect("block 0")
        .header
        .hash;
    node.terminate();

    let directory = node.datadir().join("golemdb");
    std::fs::remove_dir_all(&directory).unwrap();
    let genesis: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(example).unwrap()).unwrap();
    let config = arkiv_golemdb::GolemDbConfig {
        genesis: serde_json::from_value(genesis["config"]["arkiv"]["golemdb"].clone()).unwrap(),
        segments: segments::ALL
            .iter()
            .map(|&(name, columns)| arkiv_golemdb::SegmentSpec {
                name: name.into(),
                columns,
            })
            .collect(),
    };
    let store: HostStore =
        std::sync::Arc::new(arkiv_golemdb::GolemDbStore::open(&directory, &config).unwrap());
    let other = GenesisAccount {
        address: Address::repeat_byte(0x33),
        balance: U256::from(1),
        nonce: 0,
    };
    let spec = <arkiv_reth_chainspec::ArkivChainSpecParser as reth_cli::chainspec::ChainSpecParser>::parse(example).unwrap();
    seed_genesis(&store, genesis_hash.0, spec.fee_schedule(), [other]).unwrap();
    drop(store);

    node.restart();
    let log = refusal(&mut node).await;
    assert!(log.contains("does not hold this genesis"), "{log}");
}

/// EIP-1559 accounting end to end: the sender's balance drops by
/// `gas_used × effectiveGasPrice` — the receipt's figure, not the fee cap — the
/// block's beneficiary receives `gas_used × (effective − base_fee)`, and the
/// base-fee share is burned. Before this the executor charged the fee cap and
/// burned all of it, so receipts and balances disagreed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eip1559_sender_pays_the_effective_price_and_the_beneficiary_gets_the_tip() {
    let (_node, client, caller) = spawn_dev(DEV_KEY_0).await;
    let provider = client.provider();
    let recipient = DEV_KEY_1.parse::<PrivateKeySigner>().unwrap().address();
    const FEE_CAP: u128 = 3_000_000_000;
    const TIP: u128 = 1_000_000_000;
    const VALUE: u128 = 1_000;

    let before = provider.get_balance(caller).await.expect("eth_getBalance");
    let nonce = provider
        .get_transaction_count(caller)
        .await
        .expect("transaction count");
    // The idle dev chain sits at its 1 gwei floor, so the effective price is
    // base + tip = 2 gwei: strictly below the cap, which is the point.
    let tx = TransactionRequest::default()
        .with_to(recipient)
        .with_value(U256::from(VALUE))
        .with_nonce(nonce)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(FEE_CAP)
        .with_max_priority_fee_per_gas(TIP)
        .with_chain_id(DEV_CHAIN_ID);
    let receipt = provider
        .send_transaction(tx)
        .await
        .expect("the pool accepts the transfer")
        .get_receipt()
        .await
        .expect("mined");
    assert!(receipt.status(), "the transfer succeeds");

    let block = provider
        .get_block_by_number(receipt.block_number.expect("mined").into())
        .await
        .expect("eth_getBlockByNumber")
        .expect("the receipt's block exists");
    let base_fee = u128::from(block.header.base_fee_per_gas.expect("post-London"));
    let gas_used = u128::from(receipt.gas_used);
    let effective = receipt.effective_gas_price;
    assert_eq!(effective, (base_fee + TIP).min(FEE_CAP));
    assert!(
        effective < FEE_CAP,
        "the fee cap must not be what is charged"
    );

    let after = provider.get_balance(caller).await.expect("eth_getBalance");
    assert_eq!(
        before - after,
        U256::from(VALUE + gas_used * effective),
        "the sender pays value + gas_used × effectiveGasPrice",
    );
    // The dev miner picks a fresh random beneficiary per block, so its balance is
    // exactly this block's tips.
    let tips = provider
        .get_balance(block.header.beneficiary)
        .await
        .expect("eth_getBalance");
    assert_eq!(
        tips,
        U256::from(gas_used * (effective - base_fee)),
        "the beneficiary receives the tip; the base fee is burned",
    );
}

/// The pool end to end: a create's storage share of the base fee and the
/// value it is sent reach the balance of `ARKIV_ADDRESS`, the tip reaches the
/// beneficiary, and the sender pays value plus `gas_used × effectiveGasPrice`,
/// on the in-process store and on GolemDB.
///
/// The create carries one indexed attribute, `tag = "blue"`, and lives 1,000
/// blocks. Its record weighs 1,195 units: the empty record's 1,042 and the
/// attribute's 153, its cell and its index entry. At the dev chain's 14
/// micro-gas per unit-block that is 16.73 gas, rounded up to 17, so the pool
/// rises by `17 × base_fee + value`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pool_receives_the_storage_share_and_the_value() {
    for golemdb in [false, true] {
        let builder = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"));
        let builder = if golemdb {
            builder.golemdb_store()
        } else {
            builder
        };
        let mut node = builder.spawn();
        let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
        let caller = signer.address();
        let client = connect(&node.http_url(), signer);
        node.wait_ready(&client, READY).await;
        let store = if golemdb { "GolemDB" } else { "memory" };

        let provider = client.provider();
        const FEE_CAP: u128 = 3_000_000_000;
        const TIP: u128 = 1_000_000_000;
        const VALUE: u128 = 1_000;
        const STORAGE_GAS: u128 = 17;
        let pool_before = provider.get_balance(ARKIV_ADDRESS).await.unwrap();
        let sender_before = provider.get_balance(caller).await.unwrap();

        let tag = Attribute::from_value(
            Ident32::encode("tag").unwrap(),
            &AttributeValue::Str("blue".into()),
            true,
        )
        .unwrap();
        let receipt = IEntityRegistry::new(ARKIV_ADDRESS, provider)
            .execute(vec![Operation::create(0, 0, 1_000, 0, vec![tag])])
            .value(U256::from(VALUE))
            .max_fee_per_gas(FEE_CAP)
            .max_priority_fee_per_gas(TIP)
            .send()
            .await
            .expect("the pool accepts a create with value")
            .get_receipt()
            .await
            .expect("mined");
        assert!(receipt.status(), "{store}: the create succeeds");

        let block = provider
            .get_block_by_number(receipt.block_number.expect("mined").into())
            .await
            .unwrap()
            .expect("the receipt's block exists");
        let base_fee = u128::from(block.header.base_fee_per_gas.expect("post-London"));
        let gas_used = u128::from(receipt.gas_used);
        let effective = receipt.effective_gas_price;
        assert_eq!(effective, (base_fee + TIP).min(FEE_CAP), "{store}");

        let pool_after = provider.get_balance(ARKIV_ADDRESS).await.unwrap();
        assert_eq!(
            pool_after - pool_before,
            U256::from(STORAGE_GAS * base_fee + VALUE),
            "{store}: the pool gets base_fee × storage_gas + value",
        );
        let sender_after = provider.get_balance(caller).await.unwrap();
        assert_eq!(
            sender_before - sender_after,
            U256::from(VALUE + gas_used * effective),
            "{store}: the sender pays value + gas_used × effectiveGasPrice",
        );
        // The dev miner picks a fresh random beneficiary per block, so its
        // balance is exactly this block's tips.
        let tips = provider
            .get_balance(block.header.beneficiary)
            .await
            .unwrap();
        assert_eq!(
            tips,
            U256::from(gas_used * (effective - base_fee)),
            "{store}: the beneficiary gets the tip; the rest of the base fee is burned",
        );
    }
}

/// One entity transaction's fee, split as the executor charged it. Storage gas
/// is read off the pool: it rises by `base_fee × storage_gas` and nothing else
/// when the transaction carries no value. Purge gas is 0: iteration 1 collects
/// no purge reserve, so nothing outside shows it. Compute is the rest of
/// `gasUsed`.
struct FeeBreakdown {
    block: u64,
    gas_used: u128,
    compute_gas: u128,
    storage_gas: u128,
    purge_gas: u128,
    effective_gas_price: u128,
}

/// Send `ops` as one transaction and split what it paid.
async fn send_and_break_down(
    client: &ArkivClient<impl Provider + Clone>,
    ops: Vec<Operation>,
) -> FeeBreakdown {
    let pool_before = client.provider().get_balance(ARKIV_ADDRESS).await.unwrap();
    let receipt = client.execute(ops).await;
    break_down(client, &receipt, pool_before).await
}

/// Send `ops` as one transaction with gas limit `gas` rather than an estimate,
/// and split what it paid, whether it succeeded or not.
async fn send_with_gas_and_break_down(
    client: &ArkivClient<impl Provider + Clone>,
    ops: Vec<Operation>,
    gas: u64,
) -> (bool, FeeBreakdown) {
    let pool_before = client.provider().get_balance(ARKIV_ADDRESS).await.unwrap();
    let receipt = IEntityRegistry::new(ARKIV_ADDRESS, client.provider())
        .execute(ops)
        .gas(gas)
        .send()
        .await
        .expect("the pool accepts the transaction")
        .get_receipt()
        .await
        .expect("mined");
    (
        receipt.status(),
        break_down(client, &receipt, pool_before).await,
    )
}

/// Estimate `ops` twice in a row. A probe's writes are discarded, so the
/// second estimate sees what the first did: the same two numbers.
async fn estimate_twice(client: &ArkivClient<impl Provider + Clone>, ops: Vec<Operation>) -> u64 {
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());
    let first = registry.execute(ops.clone()).estimate_gas().await.unwrap();
    let second = registry.execute(ops).estimate_gas().await.unwrap();
    assert_eq!(first, second, "an estimate does not see the one before it");
    first
}

/// Split what the transaction behind `receipt` paid, the pool holding
/// `pool_before` before it.
async fn break_down(
    client: &ArkivClient<impl Provider + Clone>,
    receipt: &alloy_rpc_types::TransactionReceipt,
    pool_before: U256,
) -> FeeBreakdown {
    let provider = client.provider();
    let block = receipt.block_number.expect("mined");
    let base_fee = u128::from(
        provider
            .get_block_by_number(block.into())
            .await
            .unwrap()
            .expect("the receipt's block exists")
            .header
            .base_fee_per_gas
            .expect("post-London"),
    );
    let pool_credit: u128 = (provider.get_balance(ARKIV_ADDRESS).await.unwrap() - pool_before)
        .try_into()
        .unwrap();
    assert_eq!(pool_credit % base_fee, 0, "the pool gets whole storage gas");
    let gas_used = u128::from(receipt.gas_used);
    let storage_gas = pool_credit / base_fee;
    let purge_gas = 0;
    FeeBreakdown {
        block,
        gas_used,
        compute_gas: gas_used - storage_gas - purge_gas,
        storage_gas,
        purge_gas,
        effective_gas_price: receipt.effective_gas_price,
    }
}

/// GLM, the native token, has 18 decimals like ether: 1 GLM is 10¹⁸ wei.
const GLM_DECIMALS: u32 = 18;
/// The rate the fee table prints in USD, 1 GLM = 0.1 USD: one more decimal.
const USD_PER_GLM_DECIMALS: u32 = 1;

/// `amount` in units of `10^decimals`, exactly, without trailing zeros.
fn decimal(amount: u128, decimals: u32) -> String {
    let unit = 10u128.pow(decimals);
    let fraction = format!("{:0width$}", amount % unit, width = decimals as usize);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{}", amount / unit)
    } else {
        format!("{}.{fraction}", amount / unit)
    }
}

/// Storage gas for `units` kept for `blocks` at the dev chain's capacity rate
/// of 14 micro-gas per unit-block, rounded up once.
fn storage_gas(units: u128, blocks: u128) -> u128 {
    (units * blocks * 14).div_ceil(1_000_000)
}

/// A node on the in-process store or on GolemDB, a signing client for the first
/// dev key, its address, and the store's name for the output.
async fn spawn_on(
    golemdb: bool,
) -> (
    arkiv_harness::Node,
    ArkivClient<impl Provider + Clone>,
    Address,
    &'static str,
) {
    let builder = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"));
    let builder = if golemdb {
        builder.golemdb_store()
    } else {
        builder
    };
    let mut node = builder.spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    (
        node,
        client,
        caller,
        if golemdb { "GolemDB" } else { "memory" },
    )
}

/// The key `caller`'s next create mints, from its minting nonce, and the nonce.
async fn predict_key(client: &ArkivClient<impl Provider + Clone>, caller: Address) -> (u64, B256) {
    let nonce = IEntityRegistry::new(ARKIV_ADDRESS, client.provider())
        .entityNonce(caller)
        .call()
        .await
        .unwrap();
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(nonce),
        0,
    ));
    (nonce, key)
}

/// A string attribute.
fn str_attr(name: &str, value: &str, indexed: bool) -> Attribute {
    Attribute::from_value(
        Ident32::encode(name).unwrap(),
        &AttributeValue::Str(value.into()),
        indexed,
    )
    .unwrap()
}

/// The fee table's header, for `store`.
fn print_fee_header(store: &str) {
    println!(
        "[{store}] {:<9} {:>6} {:>10} {:>12} {:>12} {:>10} {:>18} {:>14} {:>14}",
        "op",
        "block",
        "gas used",
        "compute gas",
        "storage gas",
        "purge gas",
        "fee (wei)",
        "fee (GLM)",
        "fee (USD)"
    );
}

/// One row of the fee table.
fn print_fee(store: &str, op: &str, fee: &FeeBreakdown) {
    let wei = fee.gas_used * fee.effective_gas_price;
    println!(
        "[{store}] {op:<9} {:>6} {:>10} {:>12} {:>12} {:>10} {:>18} {:>14} {:>14}",
        fee.block,
        fee.gas_used,
        fee.compute_gas,
        fee.storage_gas,
        fee.purge_gas,
        wei,
        decimal(wei, GLM_DECIMALS),
        decimal(wei, GLM_DECIMALS + USD_PER_GLM_DECIMALS),
    );
}

/// A create, a patch and an extend of the entity it created, each its own
/// signed transaction, on the in-process store and on GolemDB. The test predicts the
/// entity's key before the create, and prints each transaction's fee split into
/// compute and storage gas. Run it with `--no-capture` to see the output:
///
/// ```sh
/// cargo nextest run -p arkiv-reth --test e2e --no-capture -- a_create_a_patch
/// ```
///
/// Storage follows GolemDB's size rule: a cell is `(8 + name) + (1 + value)`
/// bytes plus `c_cell` 64, and an index entry `name + 2 + value` plus `c_idx` 64.
/// - The create buys 1,283 units for its 1,000 blocks: the empty record's 1,042,
///   `tag = "blue"` indexed (cell 80, index entry 73) and `note = "hello world"`
///   unindexed (88). That is 17.96 gas, so 18.
/// - The patch adds `memo = "patched"` unindexed, 84 units, for the blocks the
///   entity has left; nothing else it writes changes size.
/// - The extend moves the expiry out by a month, 1,314,000 blocks at 2 seconds
///   (a sixth of `max_lifetime`'s six months), and buys the whole record, now
///   1,367 units, for those blocks only: 25,147.33 gas, so 25,148.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas() {
    const LIFETIME: u64 = 1_000;
    const MONTH: u64 = 1_314_000;
    for golemdb in [false, true] {
        let (mut node, client, caller, store) = spawn_on(golemdb).await;
        let attr = str_attr;
        let print = |op: &str, fee: &FeeBreakdown| print_fee(store, op, fee);

        let (nonce, key) = predict_key(&client, caller).await;
        assert!(client.get_entity(key).await.is_null(), "{store}: not yet");
        println!("[{store}] entity key {key:#x} predicted from minting nonce {nonce}");
        print_fee_header(store);

        let create = send_and_break_down(
            &client,
            vec![Operation::create(
                0,
                0,
                LIFETIME,
                0,
                vec![
                    attr("tag", "blue", true),
                    attr("note", "hello world", false),
                ],
            )],
        )
        .await;
        print("create", &create);
        let entity = client.get_entity(key).await;
        assert!(
            !entity.is_null(),
            "{store}: the create minted the predicted key"
        );
        assert_eq!(attr_value(&entity, "tag"), "blue", "{store}");
        assert_eq!(attr_value(&entity, "note"), "hello world", "{store}");
        let expires_at = hex_quantity(&entity["expiresAt"]);
        assert_eq!(expires_at, create.block + LIFETIME, "{store}");
        assert_eq!(
            create.storage_gas,
            storage_gas(1_283, u128::from(LIFETIME)),
            "{store}: the create buys its record for its lifetime"
        );

        let patch = send_and_break_down(
            &client,
            vec![Operation::patch(key, vec![attr("memo", "patched", false)])],
        )
        .await;
        print("patch", &patch);
        let entity = client.get_entity(key).await;
        assert_eq!(attr_value(&entity, "memo"), "patched", "{store}");
        assert_eq!(attr_value(&entity, "note"), "hello world", "{store}");
        assert_eq!(
            patch.storage_gas,
            storage_gas(84, u128::from(expires_at - patch.block)),
            "{store}: the patch buys its growth for the blocks left"
        );

        let extend = send_and_break_down(
            &client,
            vec![Operation::extend_expiry(key, expires_at + MONTH, 0)],
        )
        .await;
        print("extend", &extend);
        let entity = client.get_entity(key).await;
        assert_eq!(
            hex_quantity(&entity["expiresAt"]),
            expires_at + MONTH,
            "{store}: a month later"
        );
        assert_eq!(
            extend.storage_gas,
            storage_gas(1_367, u128::from(MONTH)),
            "{store}: the extend buys the record for the added blocks only"
        );
        node.terminate();
    }
}

/// Estimates price storage like inclusion, each one isolated from the one
/// before it, and a patch that shrinks an entity buys no storage, on the
/// in-process store and on GolemDB. Run with `--no-capture` for the fee table.
///
/// Each growing operation is estimated twice: a probe whose writes outlived it
/// would show the second estimate the first one's growth or extension, and price
/// its storage at 0. Then it is sent with the estimate as its gas limit, which
/// fails if the estimate left storage out.
/// - The patch adds `memo = "patched"` unindexed, 84 units, for the blocks left.
/// - The extend adds a month, 1,314,000 blocks, for the 1,367-unit record.
/// - The second patch unsets `note = "hello world"`, 88 units: storage 0, and
///   nothing refunded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn estimates_are_isolated_and_a_shrinking_patch_buys_no_storage() {
    const LIFETIME: u64 = 1_000;
    const MONTH: u64 = 1_314_000;
    for golemdb in [false, true] {
        let (mut node, client, caller, store) = spawn_on(golemdb).await;
        let (_, key) = predict_key(&client, caller).await;
        print_fee_header(store);

        let create = send_and_break_down(
            &client,
            vec![Operation::create(
                0,
                0,
                LIFETIME,
                0,
                vec![
                    str_attr("tag", "blue", true),
                    str_attr("note", "hello world", false),
                ],
            )],
        )
        .await;
        print_fee(store, "create", &create);
        let expires_at = hex_quantity(&client.get_entity(key).await["expiresAt"]);

        let grow = vec![Operation::patch(
            key,
            vec![str_attr("memo", "patched", false)],
        )];
        let estimate = estimate_twice(&client, grow.clone()).await;
        let (ok, patch) = send_with_gas_and_break_down(&client, grow, estimate).await;
        print_fee(store, "patch +", &patch);
        assert!(ok, "{store}: the estimate covers the patch");
        assert!(patch.gas_used <= u128::from(estimate), "{store}");
        assert_eq!(
            patch.storage_gas,
            storage_gas(84, u128::from(expires_at - patch.block)),
            "{store}: the patch buys its growth"
        );

        let extend = vec![Operation::extend_expiry(key, expires_at + MONTH, 0)];
        let estimate = estimate_twice(&client, extend.clone()).await;
        let (ok, extended) = send_with_gas_and_break_down(&client, extend, estimate).await;
        print_fee(store, "extend", &extended);
        assert!(ok, "{store}: the estimate covers the extend");
        assert!(extended.gas_used <= u128::from(estimate), "{store}");
        assert_eq!(
            extended.storage_gas,
            storage_gas(1_367, u128::from(MONTH)),
            "{store}: the extend buys the added blocks"
        );

        let shrink = send_and_break_down(
            &client,
            vec![Operation::patch(
                key,
                vec![Attribute::tombstone(Ident32::encode("note").unwrap())],
            )],
        )
        .await;
        print_fee(store, "patch -", &shrink);
        assert_eq!(shrink.storage_gas, 0, "{store}: a shrink buys nothing");
        let entity = client.get_entity(key).await;
        assert!(attr_value(&entity, "note").is_null(), "{store}: note unset");
        assert_eq!(attr_value(&entity, "memo"), "patched", "{store}");
        node.terminate();
    }
}

/// A create whose storage gas does not fit its gas limit reverts out of gas:
/// it pays all the gas it supplied, leaves no entity at the predicted key,
/// mints nothing, and credits the pool nothing. On the in-process store and on
/// GolemDB; run with `--no-capture` for the fee table.
///
/// The gas limit is the estimate of the same create for a short lifetime, plus
/// 10,000. The two lifetimes, 0x010000 and 0x6a0000 blocks, take one non-zero
/// calldata byte each, so the creates' compute is the same; storage is not: the
/// record's 2,216 units (the empty record's 1,042, `contentType` 94 and a
/// 1,000-byte `payload` 1,080) cost about 2,000 gas for 65,536 blocks and about
/// 215,500 for 6,946,816.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_that_does_not_fit_runs_a_create_out_of_gas() {
    const SHORT: u64 = 0x01_0000;
    const LONG: u64 = 0x6a_0000;
    let payload = Bytes::from(vec![0xab; 1_000]);
    for golemdb in [false, true] {
        let (mut node, client, caller, store) = spawn_on(golemdb).await;
        let (nonce, key) = predict_key(&client, caller).await;
        print_fee_header(store);

        let short = estimate_twice(&client, vec![create_op(SHORT, payload.clone(), vec![])]).await;
        let gas = short + 10_000;
        let (ok, create) = send_with_gas_and_break_down(
            &client,
            vec![create_op(LONG, payload.clone(), vec![])],
            gas,
        )
        .await;
        print_fee(store, "create", &create);
        assert!(!ok, "{store}: the create reverts");
        assert_eq!(
            create.gas_used,
            u128::from(gas),
            "{store}: out of gas pays all"
        );
        assert_eq!(create.storage_gas, 0, "{store}: the pool gets nothing");
        assert!(client.get_entity(key).await.is_null(), "{store}: no entity");
        assert_eq!(
            predict_key(&client, caller).await.0,
            nonce,
            "{store}: nothing minted"
        );
        node.terminate();
    }
}

/// A transaction and its receipt read back by hash from the append-only tier,
/// with every other source of the answer removed.
///
/// Getting a read to actually reach the tier takes three things, and leaving
/// out any one of them makes the test prove nothing:
///
/// * **A restart.** `eth_getTransactionByHash` consults reth's in-memory RPC
///   cache first and only calls the provider on a miss, so a freshly mined
///   transaction is always a cache hit.
/// * **A store that outlives the process.** The default in-process store does
///   not, so after a restart the tier would be empty and the provider would
///   fall back to reth. Hence Valkey.
/// * **More than `DEFAULT_PERSISTENCE_THRESHOLD` blocks**, which is 50.
///   Below it reth has written nothing to its own database, so a restart
///   loses the block entirely and there is nothing to read either way.
///
/// Ignored by default because it needs a server, like the store's own
/// conformance tests:
///
/// ```sh
/// VALKEY_URL=redis://127.0.0.1:6379 \
///   cargo nextest run -p arkiv-reth --run-ignored only -- tier
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
async fn a_transaction_and_receipt_are_read_from_the_tier() {
    let url = std::env::var("VALKEY_URL").expect(
        "this test needs the node on a store that survives a restart: \
         set VALKEY_URL (for example redis://127.0.0.1:6379)",
    );

    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .block_time("50ms")
        .valkey_store(url)
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;

    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());
    let sent = registry
        .execute(vec![create_op(
            1000,
            Bytes::from_static(b"tier read"),
            vec![],
        )])
        .send()
        .await
        .expect("send");
    let tx_hash = *sent.tx_hash();
    let receipt = sent.get_receipt().await.expect("receipt");
    assert!(receipt.status(), "the transaction must have succeeded");
    let block_number = receipt.block_number.expect("a mined receipt has a block");
    let block_hash = receipt
        .block_hash
        .expect("a mined receipt has a block hash");

    // Past the persistence threshold, so reth has the block on disk and the
    // restart below does not simply lose it.
    let target = block_number + 60;
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let tip = client
            .provider()
            .get_block_number()
            .await
            .expect("block number");
        if tip >= target {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "chain reached only {tip} of {target}; it is not producing blocks",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A shutdown, not a crash: an abrupt kill past the persistence threshold
    // leaves a partial state trie that a node with no trie cannot recover, and
    // the node would never come back up to answer the reads below. See
    // `recovery::a_crash_past_the_persistence_threshold_cannot_restart`.
    node.terminate();
    node.restart();
    node.wait_ready(&client, READY).await;
    let provider = client.provider();

    let fetched = provider
        .get_transaction_by_hash(tx_hash)
        .await
        .expect("get_transaction_by_hash")
        .expect("the transaction is found by its hash after a restart");
    assert_eq!(
        fetched.block_number,
        Some(block_number),
        "the stored row reports the block it was in",
    );
    assert_eq!(
        fetched.block_hash,
        Some(block_hash),
        "and the block hash the receipt reported",
    );

    let fetched_receipt = provider
        .get_transaction_receipt(tx_hash)
        .await
        .expect("get_transaction_receipt")
        .expect("the receipt is found by the transaction hash after a restart");
    assert_eq!(fetched_receipt.transaction_hash, tx_hash);
    assert_eq!(fetched_receipt.block_number, Some(block_number));

    // The height side of the same rows: a block number resolves to its hash
    // through the segment's ordinals, and the hash back to its number through
    // the row's name. Both answers have to survive the restart and agree with
    // what the receipt reported before it.
    let by_number = provider
        .get_block_by_number(block_number.into())
        .await
        .expect("get_block_by_number")
        .expect("the block is found by its number after a restart");
    assert_eq!(
        by_number.header.hash, block_hash,
        "the height resolves to the hash the receipt reported",
    );
    let by_hash = provider
        .get_block_by_hash(block_hash)
        .await
        .expect("get_block_by_hash")
        .expect("the block is found by its hash after a restart");
    assert_eq!(
        by_hash.header.number, block_number,
        "and the hash resolves back to the height",
    );

    // `eth_getLogs` is what reaches the transaction *number* side of the tier:
    // it finds a log by block and index and then asks the provider for the
    // transaction by its number to report the hash. reth's other callers of
    // that method are staged-sync stages, which read reth's own provider
    // rather than this one, so this request is the only path to it.
    let logs = provider
        .get_logs(
            &alloy_rpc_types::Filter::new()
                .from_block(block_number)
                .to_block(block_number),
        )
        .await
        .expect("eth_getLogs");
    assert!(
        !logs.is_empty(),
        "the create emitted an EntityOperation log, so the block has logs",
    );
    assert!(
        logs.iter().all(|log| log.transaction_hash == Some(tx_hash)),
        "every log in the block belongs to the create, by the hash the number resolved to",
    );
}
