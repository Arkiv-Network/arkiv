//! The committed prefill fixture, end to end: prefill and finalize it with the
//! built `arkiv-reth prefill`, then start a node on that store and read the
//! prefilled entities back over RPC, and join a second node from a copy of
//! the store, as `docs/prefill-runbook.md` describes.
//!
//! The fixture is `crates/arkiv-prefill/fixtures/small`: two archives, six
//! entities, and the block 0 root and hash they must produce
//! (`expected.json`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use alloy_primitives::B256;
use alloy_provider::Provider;
use arkiv_harness::{NodeBuilder, connect_reader, hex_quantity};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_arkiv-reth");
const READY: Duration = Duration::from_secs(90);
const ALICE: &str = "0x1111111111111111111111111111111111111111";
const BOB: &str = "0x2222222222222222222222222222222222222222";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/arkiv-prefill/fixtures/small")
}

/// A copy of the fixture in `to`, which the prefill can write into: the
/// finalize rewrites its genesis file.
fn copy(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// `arkiv-reth prefill` on the copy in `dir`, with `extra` flags; its output.
fn prefill(dir: &Path, extra: &[&str]) -> String {
    let output = Command::new(BIN)
        .arg("prefill")
        .arg("--chain")
        .arg(dir.join("genesis.json"))
        .arg("--archives")
        .arg(dir.join("set.json"))
        .arg("--store")
        .arg(dir.join("golemdb"))
        .args(extra)
        .output()
        .expect("run arkiv-reth prefill");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "arkiv-reth prefill {extra:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// The hash of block `number` on the node `client` reads.
async fn block_hash(client: &arkiv_harness::ArkivClient<impl Provider>, number: u64) -> B256 {
    client
        .provider()
        .get_block_by_number(number.into())
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("block {number}"))
        .header
        .hash
}

/// A copy of the fixture in a new directory, prefilled and finalized: its
/// `genesis.json` carries block 0's root, its `golemdb` is block 0's store.
fn prefilled() -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    copy(&fixture(), work.path());
    prefill(work.path(), &[]);
    prefill(work.path(), &["--finalize"]);
    work
}

/// The fixture's block 0 root and hash, from `expected.json`.
fn expected() -> (B256, B256) {
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture().join("expected.json")).unwrap())
            .unwrap();
    let field = |name: &str| expected[name].as_str().unwrap().parse().unwrap();
    (field("root"), field("blockZeroHash"))
}

/// The entity at `key` (a byte, repeated), as `arkiv_getEntity` returns it.
async fn entity(client: &arkiv_harness::ArkivClient<impl Provider>, key: u8) -> Value {
    client.get_entity(B256::repeat_byte(key)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_starts_on_the_prefilled_fixture() {
    let work = prefilled();
    let dir = work.path();

    // Block 0 is the fixture's, wherever it is built.
    let (root, hash) = expected();
    let genesis: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("genesis.json")).unwrap()).unwrap();
    assert_eq!(
        genesis["config"]["arkiv"]["prefilledBlockZeroStateRoot"].as_str(),
        Some(root.to_string().as_str())
    );

    // The node starts on the store under the finalized genesis file, and the
    // chain moves on from it.
    let mut node = NodeBuilder::new(BIN)
        .chain(dir.join("genesis.json"))
        .golemdb_path(dir.join("golemdb"))
        .spawn();
    let client = connect_reader(&node.http_url());
    node.wait_ready(&client, READY).await;
    let block_zero = client
        .provider()
        .get_block_by_number(0.into())
        .await
        .unwrap()
        .expect("block 0");
    assert_eq!(block_zero.header.hash, hash);
    assert_eq!(block_zero.header.state_root, root);
    client.wait_for_block(2, READY).await;

    // Every prefilled entity is there, created at block 0.
    assert_eq!(client.entity_count(None, None).await, 6);
    for key in [0xa1, 0xa2, 0xa3, 0xb1, 0xb2, 0xb3] {
        let entity = entity(&client, key).await;
        assert_eq!(hex_quantity(&entity["createdAt"]), 0, "{key:#x}: {entity}");
    }
    // Created by Alice, owned by Bob: the prefill's transfer.
    let transferred = entity(&client, 0xa2).await;
    assert_eq!(
        transferred["creator"]
            .as_str()
            .map(str::to_lowercase)
            .as_deref(),
        Some(ALICE)
    );
    assert_eq!(
        transferred["owner"]
            .as_str()
            .map(str::to_lowercase)
            .as_deref(),
        Some(BOB)
    );
    // An expiry far past max_lifetime, kept as the archive gave it.
    assert_eq!(
        hex_quantity(&entity(&client, 0xb1).await["expiresAt"]),
        1_000_000_000
    );
    // And queryable by attribute, as any entity is.
    assert_eq!(
        client.entity_count(Some("kind = str('note')"), None).await,
        3
    );
}

/// The runbook's join: a second node starts from a copy of the store taken
/// right after the finalize, while the first is already past block 0. It
/// peers with the first, follows it (reth's RPC consensus stands in for a
/// consensus client), and ends up on the same chain with the prefilled
/// entities.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_joins_from_a_copy_of_the_block_zero_store() {
    let work = prefilled();
    let dir = work.path();
    let joiner_store = dir.join("golemdb-block0");
    std::fs::create_dir_all(&joiner_store).unwrap();
    copy(&dir.join("golemdb"), &joiner_store);

    let mut first = NodeBuilder::new(BIN)
        .chain(dir.join("genesis.json"))
        .golemdb_path(dir.join("golemdb"))
        .http_api("eth,net,admin")
        .spawn();
    let first_client = connect_reader(&first.http_url());
    first.wait_ready(&first_client, READY).await;
    first_client.wait_for_block(5, READY).await;
    let info: Value = first_client
        .provider()
        .raw_request("admin_nodeInfo".into(), serde_json::json!([]))
        .await
        .unwrap();
    let enode = info["enode"].as_str().expect("an enode").to_owned();

    let mut joiner = NodeBuilder::new(BIN)
        .dev(false)
        .chain(dir.join("genesis.json"))
        .golemdb_path(&joiner_store)
        .arg("--trusted-peers")
        .arg(enode)
        .arg("--debug.rpc-consensus-url")
        .arg(first.http_url())
        .spawn();
    let joiner_client = connect_reader(&joiner.http_url());
    joiner.wait_ready(&joiner_client, READY).await;
    let target = first_client.block_number().await + 3;
    joiner_client.wait_for_block(target, READY).await;

    // The same chain: block 0 is the fixture's on both, and so is every block
    // the joiner followed.
    let (_, hash) = expected();
    for number in [0, 1, target] {
        let ours = block_hash(&joiner_client, number).await;
        assert_eq!(
            ours,
            block_hash(&first_client, number).await,
            "block {number}"
        );
        if number == 0 {
            assert_eq!(ours, hash);
        }
    }
    let peers: String = joiner_client
        .provider()
        .raw_request("net_peerCount".into(), serde_json::json!([]))
        .await
        .unwrap();
    assert_ne!(peers, "0x0", "the joiner peers with the first node");

    // The prefilled entities, served by the joiner.
    assert_eq!(joiner_client.entity_count(None, None).await, 6);
    let entity = entity(&joiner_client, 0xb1).await;
    assert_eq!(hex_quantity(&entity["createdAt"]), 0, "{entity}");
}
