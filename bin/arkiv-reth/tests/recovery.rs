//! Crash recovery (harness test T3): kill the node mid-run and restart it on the
//! same datadir — the committed entity state and query index resume from the safe
//! head, and the chain keeps producing blocks and accepting writes.
//!
//! Black-box over the real binary: the [`arkiv_harness`] node is killed (process
//! stopped, datadir kept) and restarted on the same ports, then driven again over
//! RPC.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_network::TransactionBuilder;
use alloy_primitives::{B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::{TransactionReceipt, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, AttributeValue, Ident32, Operation};
use arkiv_harness::{
    ArkivClient, DEV_CHAIN_ID, DEV_KEY_0, DEV_KEY_1, EntityCreationNonce, NodeBuilder, connect,
    derive_entity_address, hex_quantity, result_keys,
};
use arkiv_interfaces::constants::rpc_error_codes;

const READY: Duration = Duration::from_secs(90);

/// Blocks to let pass before a kill, to clear reth's
/// `DEFAULT_PERSISTENCE_THRESHOLD` of 50 — the depth at which reth's own
/// persistence would have run, had this node not replaced it with its own.
const PERSISTED: u64 = 60;

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

/// Wait until the store holds the block `receipt` is in.
///
/// The store commits a block just after reth makes it canonical, and reth
/// serves the receipt from that moment, so a read straight after it can still
/// see the block before. On a healthy chain the store lags reth by at most that
/// one block, so once the next block is out, the store has this one.
async fn wait_for_store(client: &ArkivClient<impl Provider>, receipt: &TransactionReceipt) {
    let mined = receipt.block_number.expect("mined");
    client
        .wait_for_block(mined + 1, Duration::from_secs(60))
        .await;
}

fn attrs(rank: u64, team: &str) -> Vec<Attribute> {
    let attr = |name: &str, value: AttributeValue| {
        Attribute::from_value(Ident32::encode(name).unwrap(), &value, true).unwrap()
    };
    vec![
        attr("rank", AttributeValue::u256_from_u64(rank)),
        attr("team", AttributeValue::Str(team.into())),
    ]
}

fn pruning_map_files(root: &Path) -> Vec<PathBuf> {
    let mut matches = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return matches;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            matches.extend(pruning_map_files(&path));
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("arkiv-pruning.db"))
        {
            matches.push(path);
        }
    }
    matches
}

async fn wait_for_physical_purge(
    client: &arkiv_harness::ArkivClient<impl alloy_provider::Provider>,
    key: B256,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while client.debug_entity_exists(key).await {
        assert!(
            std::time::Instant::now() < deadline,
            "the entity was not physically purged within 30 seconds"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Entity state and the query index survive a shutdown and restart, and the
/// chain goes on producing blocks afterwards.
///
/// Two things this needs that the rest of the file does not:
///
/// * **A store that outlives the process.** Arkiv's state is not in the
///   datadir; by default it is in-process, so after a restart there is simply
///   nothing to find and the question the test asks is unanswerable. Hence
///   GolemDB on disk, which is also what a real deployment runs.
/// * **More than `DEFAULT_PERSISTENCE_THRESHOLD` blocks**, which is 50. Below
///   it reth has written nothing to its own database, and a restart therefore
///   never reaches the startup checks or the catch-up this is here to cover —
///   both of the faults it guards against (the trie stages rejecting a header
///   that carries GolemDB's root, and a tag becoming readable before the head
///   covered it) are invisible under 50 blocks.
///
/// This is the graceful restart — SIGTERM, the one an operator performs.
///
/// Ignored by default because it is slow: it waits out more than 50 blocks.
///
/// ```sh
/// cargo nextest run -p arkiv-reth --run-ignored only -- restart
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: waits out reth's persistence threshold; run with --ignored"]
async fn state_and_index_survive_shutdown_and_restart() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .golemdb_store()
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;

    // Create two entities with attributes, so there is committed state and index.
    client
        .execute(vec![
            create_op(1000, Bytes::from_static(b"alpha"), attrs(10, "red")),
            create_op(1000, Bytes::from_static(b"beta"), attrs(20, "blue")),
        ])
        .await;
    let key0 = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    let key1 = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(1),
        0,
    ));
    let created = hex_quantity(&client.get_entity(key0).await["createdAt"]);

    // Past reth's persistence threshold, so the create block is on disk and the
    // restart below exercises the startup path rather than simply losing it.
    client
        .wait_for_block(created + PERSISTED, Duration::from_secs(60))
        .await;
    let count_before = client.entity_count(None, None).await;
    let entity_before = client.get_entity(key0).await;
    assert_eq!(count_before, 2);

    // Shut down (datadir kept), then restart on the same ports/datadir.
    node.terminate();
    // A brief pause so the OS releases the listening ports before the rebind.
    tokio::time::sleep(Duration::from_secs(1)).await;
    node.restart();
    node.wait_ready(&client, READY).await;

    // The safe head resumed: the entities and their index survive byte-identical.
    assert_eq!(
        client.entity_count(None, None).await,
        2,
        "entity count survives restart"
    );
    let entity_after = client.get_entity(key0).await;
    assert_eq!(
        attr_value(&entity_after, "payload"),
        attr_value(&entity_before, "payload"),
        "payload survives"
    );
    assert_eq!(
        entity_after["createdAt"], entity_before["createdAt"],
        "create block survives",
    );
    assert_eq!(
        result_keys(&client.query("rank = u256(20)", 100, None).await),
        BTreeSet::from([format!("{key1:#x}")]),
        "the query index survives restart",
    );

    // The chain keeps producing blocks and accepts new writes after recovery.
    // Ten of them rather than one or two: a restarted node that cannot read its
    // own state back fails while *building* a payload, not at startup, so the
    // evidence that it recovered is sustained block production.
    let tip = client.block_number().await;
    client
        .wait_for_block(tip + 10, Duration::from_secs(60))
        .await;
    client
        .execute(vec![create_op(1000, Bytes::from_static(b"gamma"), vec![])])
        .await;
    assert_eq!(
        client.entity_count(None, None).await,
        3,
        "new writes work after recovery"
    );
}

/// On GolemDB, entities and balances survive a shutdown and restart, and the
/// node goes on producing blocks and taking writes afterwards.
///
/// Like the restart above, and past the persistence threshold for the same
/// reasons, but GolemDB needs no server, so this one always runs. It reads
/// entities by key: GolemDB does not answer queries or counts yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golemdb_state_and_balances_survive_shutdown_and_restart() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .golemdb_store()
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    assert!(
        node.datadir().join("golemdb").is_dir(),
        "the node runs on GolemDB"
    );
    let provider = client.provider();
    let key = |nonce| {
        B256::from(derive_entity_address(
            DEV_CHAIN_ID,
            &caller.into_array(),
            EntityCreationNonce::new(nonce),
            0,
        ))
    };

    // Two entities, then a patch of the first and a delete of the second.
    client
        .execute(vec![
            create_op(1000, Bytes::from_static(b"alpha"), attrs(10, "red")),
            create_op(1000, Bytes::from_static(b"beta"), attrs(20, "blue")),
        ])
        .await;
    let changed = client
        .execute(vec![
            patch_payload(key(0), Bytes::from_static(b"v2")),
            Operation::delete(key(1)),
        ])
        .await;
    wait_for_store(&client, &changed).await;
    let entity_before = client.get_entity(key(0)).await;
    assert_eq!(
        attr_value(&entity_before, "payload"),
        "0x7632",
        "the patch landed"
    ); // "v2"
    assert!(
        client.get_entity(key(1)).await.is_null(),
        "the delete landed"
    );

    // GolemDB keeps only the head's state, so a read at an older block is
    // refused as such, not answered from the head or failed as internal.
    let created = hex_quantity(&entity_before["createdAt"]);
    let past = provider
        .raw_request::<_, serde_json::Value>("arkiv_getEntity".into(), (key(0), created))
        .await
        .expect_err("a read at a past block");
    assert_eq!(
        past.as_error_resp().map(|error| error.code),
        Some(rpc_error_codes::BLOCK_UNAVAILABLE.into()),
        "{past}"
    );

    // A plain transfer, so there is a balance to read back.
    let recipient = DEV_KEY_1.parse::<PrivateKeySigner>().unwrap().address();
    let funded = provider
        .get_balance(recipient)
        .await
        .expect("eth_getBalance");
    let transfer = TransactionRequest::default()
        .with_to(recipient)
        .with_value(U256::from(1_000))
        .with_nonce(provider.get_transaction_count(caller).await.expect("nonce"))
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(3_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000_000)
        .with_chain_id(DEV_CHAIN_ID);
    let receipt = provider
        .send_transaction(transfer)
        .await
        .expect("the pool accepts the transfer")
        .get_receipt()
        .await
        .expect("mined");
    assert!(receipt.status(), "the transfer succeeds");
    wait_for_store(&client, &receipt).await;
    let balance_before = provider
        .get_balance(recipient)
        .await
        .expect("eth_getBalance");
    assert_eq!(balance_before, funded + U256::from(1_000));

    let mined = receipt.block_number.expect("mined");
    client
        .wait_for_block(mined + PERSISTED, Duration::from_secs(60))
        .await;
    node.terminate();
    tokio::time::sleep(Duration::from_secs(1)).await;
    node.restart();
    node.wait_ready(&client, READY).await;

    assert_eq!(
        client.get_entity(key(0)).await,
        entity_before,
        "the patched entity survives"
    );
    assert!(
        client.get_entity(key(1)).await.is_null(),
        "the deleted entity stays deleted"
    );
    assert_eq!(
        provider
            .get_balance(recipient)
            .await
            .expect("eth_getBalance"),
        balance_before,
        "the balance survives"
    );

    // Sustained block production and a new write, as above.
    let tip = client.block_number().await;
    client
        .wait_for_block(tip + 10, Duration::from_secs(60))
        .await;
    let gamma = client
        .execute(vec![create_op(1000, Bytes::from_static(b"gamma"), vec![])])
        .await;
    wait_for_store(&client, &gamma).await;
    assert!(
        !client.get_entity(key(2)).await.is_null(),
        "new writes work after recovery"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pruning_schedule_survives_kill_and_restart() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth")).spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;

    client
        .execute(vec![create_op(
            30,
            Bytes::from_static(b"persistent-pruning"),
            vec![],
        )])
        .await;
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    let entity = client.get_entity(key).await;
    let created_at = hex_quantity(&entity["createdAt"]);
    let expires_at = hex_quantity(&entity["expiresAt"]);
    client
        .wait_for_block(created_at + 20, Duration::from_secs(30))
        .await;

    node.kill();
    tokio::time::sleep(Duration::from_secs(1)).await;
    node.restart();
    node.wait_ready(&client, READY).await;
    client
        .wait_for_block(expires_at + 5, Duration::from_secs(30))
        .await;
    wait_for_physical_purge(&client, key).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pruning_schedule_rebuilds_from_chain_history() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth")).spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;

    client
        .execute(vec![create_op(
            40,
            Bytes::from_static(b"replayed-pruning"),
            vec![],
        )])
        .await;
    let key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &caller.into_array(),
        EntityCreationNonce::new(0),
        0,
    ));
    let entity = client.get_entity(key).await;
    let created_at = hex_quantity(&entity["createdAt"]);
    let expires_at = hex_quantity(&entity["expiresAt"]);
    client
        .wait_for_block(created_at + 20, Duration::from_secs(30))
        .await;

    node.kill();
    let map_files = pruning_map_files(node.datadir());
    assert!(!map_files.is_empty(), "the pruning map exists before loss");
    for path in map_files {
        std::fs::remove_file(&path)
            .unwrap_or_else(|error| panic!("remove {}: {error}", path.display()));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    node.restart();
    node.wait_ready(&client, READY).await;
    client
        .wait_for_block(expires_at + 5, Duration::from_secs(30))
        .await;
    wait_for_physical_purge(&client, key).await;
}

/// A transaction sent right after the previous one's receipt lands too. reth
/// serves a block's receipts as soon as the block is canonical, a moment before
/// the store commits it, so the pool must not check the next transaction
/// against the state from before that block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golemdb_back_to_back_transactions_all_land() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .golemdb_store()
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    let provider = client.provider();
    let recipient = DEV_KEY_1.parse::<PrivateKeySigner>().unwrap().address();

    for sent in 0..10 {
        let transfer = TransactionRequest::default()
            .with_to(recipient)
            .with_value(U256::from(1))
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(3_000_000_000)
            .with_max_priority_fee_per_gas(1_000_000_000)
            .with_chain_id(DEV_CHAIN_ID);
        let receipt = provider
            .send_transaction(transfer)
            .await
            .expect("the pool accepts the transfer")
            .with_timeout(Some(Duration::from_secs(30)))
            .get_receipt()
            .await
            .unwrap_or_else(|error| panic!("transfer {sent} was not mined: {error}"));
        assert!(receipt.status(), "transfer {sent} succeeds");
    }
}
