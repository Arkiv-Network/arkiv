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

use alloy_primitives::{B256, Bytes};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, AttributeValue, Ident32, Operation};
use arkiv_harness::{
    DEV_CHAIN_ID, DEV_KEY_0, EntityCreationNonce, NodeBuilder, connect, derive_entity_address,
    hex_quantity, result_keys,
};

const READY: Duration = Duration::from_secs(90);

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

fn attrs(rank: u64, team: &str) -> Vec<Attribute> {
    let attr = |name: &str, value: AttributeValue| {
        Attribute::from_value(Ident32::encode(name).unwrap(), &value).unwrap()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_and_index_survive_kill_and_restart() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth")).spawn();
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

    // Let the chain advance well past the create block so it is persisted below the
    // in-memory tip (the safe head that survives an abrupt kill).
    client
        .wait_for_block(created + 20, Duration::from_secs(30))
        .await;
    let count_before = client.entity_count(None, None).await;
    let entity_before = client.get_entity(key0).await;
    assert_eq!(count_before, 2);

    // Crash: stop the process (datadir kept), then restart on the same ports/datadir.
    node.kill();
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
        entity_after["payload"], entity_before["payload"],
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
    let tip = client.block_number().await;
    client
        .wait_for_block(tip + 2, Duration::from_secs(30))
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
