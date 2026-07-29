//! Crash recovery (harness test T3): kill the node mid-run and restart it on the
//! same datadir — the committed entity state and query index resume from the safe
//! head, and the chain keeps producing blocks and accepting writes.
//!
//! Black-box over the real binary: the [`arkiv_harness`] node is killed (process
//! stopped, datadir kept) and restarted on the same ports, then driven again over
//! RPC.

use std::collections::BTreeSet;
use std::time::Duration;

use alloy_primitives::{B256, Bytes};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, AttributeValue, Ident32, Mime128, Operation};
use arkiv_harness::{
    DEV_CHAIN_ID, DEV_KEY_0, NodeBuilder, connect, derive_entity_key, result_keys,
};

const READY: Duration = Duration::from_secs(90);

fn text_plain_mime() -> Mime128 {
    Mime128::encode("text/plain").expect("valid mime")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_and_index_survive_kill_and_restart() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-node")).spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let client = connect(&node.http_url(), signer);
    client.wait_ready(READY).await;

    // Create two entities with attributes, so there is committed state and index.
    client
        .execute(vec![
            Operation::create(
                1000,
                Bytes::from_static(b"alpha"),
                text_plain_mime(),
                attrs(10, "red"),
            ),
            Operation::create(
                1000,
                Bytes::from_static(b"beta"),
                text_plain_mime(),
                attrs(20, "blue"),
            ),
        ])
        .await;
    let key0 = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 0));
    let key1 = B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), 1));
    let created = client.get_entity(key0).await["createdAtBlock"]
        .as_u64()
        .unwrap();

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
    client.wait_ready(READY).await;

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
        entity_after["createdAtBlock"], entity_before["createdAtBlock"],
        "create block survives",
    );
    assert_eq!(
        result_keys(&client.query("rank = 20", 100, None).await),
        BTreeSet::from([format!("{key1:#x}")]),
        "the query index survives restart",
    );

    // The chain keeps producing blocks and accepts new writes after recovery.
    let tip = client.block_number().await;
    client
        .wait_for_block(tip + 2, Duration::from_secs(30))
        .await;
    client
        .execute(vec![Operation::create(
            1000,
            Bytes::from_static(b"gamma"),
            text_plain_mime(),
            vec![],
        )])
        .await;
    assert_eq!(
        client.entity_count(None, None).await,
        3,
        "new writes work after recovery"
    );
}
