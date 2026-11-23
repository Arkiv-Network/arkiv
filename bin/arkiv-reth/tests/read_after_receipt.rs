//! A read made right after a receipt sees the write that receipt describes.
//!
//! reth serves a block's receipt a moment before the node commits the block's
//! Arkiv state to the store. A read at the tip waits for that commit, so a
//! client can read what it wrote as soon as it holds the receipt.
//!
//! The harness's own `execute` waits for a receipt through a watcher that
//! polls far more slowly than that moment lasts. This test asks for the
//! receipt every millisecond instead, so its reads land inside it.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alloy_primitives::B256;
use alloy_provider::Provider;
use alloy_rpc_types::TransactionReceipt;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::{Attribute, AttributeValue, IEntityRegistry, Ident32, Operation};
use arkiv_harness::{
    ARKIV_ADDRESS, ArkivClient, DEV_KEY_0, NodeBuilder, connect, hex_quantity, result_keys,
};

/// How long to wait for a freshly-spawned node's RPC to answer.
const READY: Duration = Duration::from_secs(90);

/// How often the receipt is asked for.
const RECEIPT_POLL: Duration = Duration::from_millis(1);

/// How many creates the test reads back. A read that does not wait misses
/// every one of them.
const ROUNDS: usize = 20;

fn create_op() -> Operation {
    let payload = Attribute::from_value(
        Ident32::encode("payload").unwrap(),
        &AttributeValue::Bytes(b"read-after-receipt".to_vec()),
        false,
    )
    .unwrap();
    Operation::create(0, 0, 100_000, 0, vec![payload])
}

/// The key of the entity the transaction behind `receipt` created.
fn created_key(receipt: &TransactionReceipt) -> B256 {
    receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.inner.address == ARKIV_ADDRESS)
        .find_map(|log| IEntityRegistry::EntityCreated::decode_log(&log.inner).ok())
        .expect("an EntityCreated log")
        .entityKey
}

/// Send one create and return its receipt as soon as the node serves it.
async fn create_and_wait(client: &ArkivClient<impl Provider + Clone>) -> TransactionReceipt {
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());
    let sent = registry
        .execute(vec![create_op()])
        .gas(8_000_000)
        .gas_price(1_000_000_000)
        .send()
        .await
        .expect("submit the create");
    let hash = *sent.tx_hash();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(receipt) = client
            .provider()
            .get_transaction_receipt(hash)
            .await
            .expect("eth_getTransactionReceipt")
        {
            assert!(receipt.status(), "the create reverted");
            return receipt;
        }
        assert!(Instant::now() < deadline, "no receipt for {hash}");
        tokio::time::sleep(RECEIPT_POLL).await;
    }
}

/// Create an entity, and on seeing its receipt read it back at the tip, by key
/// and by query. Both reads must find it, every round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_right_after_a_receipt_sees_its_write_on_golemdb() {
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .golemdb_store()
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;

    for round in 0..ROUNDS {
        let receipt = create_and_wait(&client).await;
        let key = created_key(&receipt);
        let block = receipt.block_number.expect("a mined receipt has a block");

        // Both at once, so each starts as close to the receipt as it can.
        let by_key = format!("$key = key({key:#x})");
        let (entity, query) = tokio::join!(client.get_entity(key), client.query(&by_key, 10, None));

        assert!(
            !entity.is_null(),
            "round {round}: arkiv_getEntity found no entity {key} right after the receipt \
             for block {block}",
        );
        let answered_for = hex_quantity(&query["blockNumber"]);
        assert!(
            answered_for >= block,
            "round {round}: arkiv_query answered for block {answered_for}, before the \
             receipt's block {block}",
        );
        assert_eq!(
            result_keys(&query),
            BTreeSet::from([format!("{key:#x}")]),
            "round {round}: arkiv_query answered for block {answered_for} without the \
             entity created in block {block}",
        );
    }
}
