//! Independent block execution must rebuild custom records and the same outer root.
use alloy_primitives::{B256, Bytes};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, AttributeValue, Ident32, Operation};
use arkiv_harness::{DEV_KEY_0, NodeBuilder, connect, connect_reader};
use std::{process::Command, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn importing_blocks_reconstructs_the_authenticated_state() {
    let ready = Duration::from_secs(120);
    let mut source = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .http_api("eth,net,web3,txpool,debug")
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let client = connect(&source.http_url(), signer);
    source.wait_ready(&client, ready).await;
    let age = Attribute::from_value(
        Ident32::encode("age").unwrap(),
        &AttributeValue::u256_from_u64(35),
    )
    .unwrap();
    let receipt = client
        .execute(vec![Operation::create(0, 0, 1000, 0, vec![age])])
        .await;
    let height = receipt.block_number.unwrap();
    let source_block = client
        .provider()
        .get_block_by_number(height.into())
        .await
        .unwrap()
        .unwrap();
    let expected = client.query_at("age >= u256(30)", height).await;
    let keys = arkiv_harness::result_keys(&expected);
    assert_eq!(keys.len(), 1);
    let key: B256 = keys.first().unwrap().parse().unwrap();
    let entity = client.get_entity_at(key, height).await;
    let mut blocks = Vec::new();
    for number in 1..=height {
        let raw: Bytes = client
            .provider()
            .raw_request("debug_getRawBlock".into(), (format!("0x{number:x}"),))
            .await
            .unwrap();
        blocks.extend_from_slice(&raw);
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("blocks.rlp");
    std::fs::write(&path, blocks).unwrap();
    let mut replica = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .dev(false)
        .spawn();
    let reader = connect_reader(&replica.http_url());
    replica.wait_ready(&reader, ready).await;
    replica.kill();
    let output = Command::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .args([
            "import",
            "--chain",
            "dev",
            "--fail-on-invalid-block",
            "--datadir",
        ])
        .arg(replica.datadir())
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "import failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    replica.restart();
    replica.wait_ready(&reader, ready).await;
    let replayed = reader
        .provider()
        .get_block_by_number(height.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source_block.header.hash, replayed.header.hash);
    assert_eq!(source_block.header.state_root, replayed.header.state_root);
    assert_eq!(reader.get_entity_at(key, height).await, entity);
    let actual = reader.query_at("age >= u256(30)", height).await;
    assert_eq!(arkiv_harness::result_keys(&actual), keys);
}
