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

use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::{Attribute, AttributeValue, IEntityRegistry, Ident32, Operation};
use arkiv_harness::{ARKIV_ADDRESS, DEV_KEY_0, NodeBuilder, connect, connect_reader, hex_quantity};
use serde_json::{Value, json};

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

/// `arkiv-reth prefill` with the genesis file `chain` and the archives and
/// store in `dir`, with `extra` flags: its output, or its output and errors if
/// it failed.
fn try_prefill(chain: &Path, dir: &Path, extra: &[&str]) -> Result<String, String> {
    let output = Command::new(BIN)
        .arg("prefill")
        .arg("--chain")
        .arg(chain)
        .arg("--archives")
        .arg(dir.join("set.json"))
        .arg("--store")
        .arg(dir.join("golemdb"))
        .args(extra)
        .output()
        .expect("run arkiv-reth prefill");
    let stdout = String::from_utf8(output.stdout).unwrap();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// `arkiv-reth prefill` on the copy in `dir`, with `extra` flags; its output.
fn prefill(dir: &Path, extra: &[&str]) -> String {
    try_prefill(&dir.join("genesis.json"), dir, extra)
        .unwrap_or_else(|error| panic!("arkiv-reth prefill {extra:?} failed:\n{error}"))
}

/// The JSON file at `path`, changed by `edit`.
fn edit_json(path: &Path, edit: impl FnOnce(&mut Value)) {
    let mut json: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    edit(&mut json);
    std::fs::write(path, json.to_string()).unwrap();
}

/// A copy of the fixture in a new directory, not yet prefilled.
fn fixture_copy() -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    copy(&fixture(), work.path());
    work
}

/// The fixture's second archive's second part, with a line no prefill
/// accepts, so a prefill stops after the first archive.
fn break_second_archive(dir: &Path) -> String {
    let part = dir.join("A1/entities-0002.jsonl");
    let good = std::fs::read_to_string(&part).unwrap();
    std::fs::write(&part, format!("{good}not an entity\n")).unwrap();
    good
}

/// Start a node on `store` under the genesis file `chain`, which must refuse
/// to start; its log.
async fn refusal(chain: &Path, store: &Path) -> String {
    let mut node = NodeBuilder::new(BIN)
        .chain(chain)
        .golemdb_path(store)
        .spawn();
    let status = node.wait_exit(READY).await;
    assert!(!status.success(), "the node exits with an error ({status})");
    std::fs::read_to_string(node.log_path()).unwrap()
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
/// `funded` are added to the copy's `alloc`, so they can send transactions;
/// with any, block 0 is no longer `expected.json`'s.
fn prefilled(funded: &[Address]) -> tempfile::TempDir {
    let work = fixture_copy();
    if !funded.is_empty() {
        edit_json(&work.path().join("genesis.json"), |genesis| {
            for address in funded {
                genesis["alloc"][address.to_string()] =
                    json!({ "balance": "0x3635c9adc5dea00000" });
            }
        });
    }
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
    let work = prefilled(&[]);
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
/// entities and the one created after block 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_joins_from_a_copy_of_the_block_zero_store() {
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let work = prefilled(&[signer.address()]);
    let dir = work.path();
    let joiner_store = dir.join("golemdb-block0");
    std::fs::create_dir_all(&joiner_store).unwrap();
    copy(&dir.join("golemdb"), &joiner_store);

    let mut first = NodeBuilder::new(BIN)
        .chain(dir.join("genesis.json"))
        .golemdb_path(dir.join("golemdb"))
        .http_api("eth,net,admin")
        .spawn();
    let first_client = connect(&first.http_url(), signer);
    first.wait_ready(&first_client, READY).await;

    // An entity created after block 0, then five more blocks, all before the
    // joiner starts.
    let payload = Attribute::from_value(
        Ident32::encode("payload").unwrap(),
        &AttributeValue::Bytes(b"after block 0".to_vec()),
        false,
    )
    .unwrap();
    let receipt = first_client
        .execute(vec![Operation::create(0, 0, 100_000, 0, vec![payload])])
        .await;
    let created = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.inner.address == ARKIV_ADDRESS)
        .find_map(|log| IEntityRegistry::EntityCreated::decode_log(&log.inner).ok())
        .expect("an EntityCreated log")
        .entityKey;
    let created_in = receipt.block_number.expect("a mined receipt");
    first_client.wait_for_block(created_in + 5, READY).await;
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

    // The same chain: block 0, the block with the create, and every block the
    // joiner followed.
    for number in [0, 1, created_in, target] {
        assert_eq!(
            block_hash(&joiner_client, number).await,
            block_hash(&first_client, number).await,
            "block {number}"
        );
    }
    let peers: String = joiner_client
        .provider()
        .raw_request("net_peerCount".into(), serde_json::json!([]))
        .await
        .unwrap();
    assert_ne!(peers, "0x0", "the joiner peers with the first node");

    // The prefilled entities and the later one, served by the joiner as the
    // first node serves them.
    assert_eq!(joiner_client.entity_count(None, None).await, 7);
    let entity = entity(&joiner_client, 0xb1).await;
    assert_eq!(hex_quantity(&entity["createdAt"]), 0, "{entity}");
    let later = joiner_client.get_entity(created).await;
    assert_eq!(hex_quantity(&later["createdAt"]), created_in, "{later}");
    assert_eq!(later, first_client.get_entity(created).await);
}

/// Each store an operator can wrongly start a node on is refused with the
/// message the runbook's "When the node refuses to start" table quotes,
/// before the node runs anything. (A store made with other GolemDB settings is
/// in `e2e.rs`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_refuses_a_store_that_does_not_fit_its_genesis_file() {
    let work = prefilled(&[]);
    let dir = work.path();
    let finalized = dir.join("genesis.json");
    let store = dir.join("golemdb");
    // The genesis file as it was before `--finalize`: the fixture's.
    let before_finalize = fixture().join("genesis.json");

    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    // The finalized file, with alloc edited after the prefill.
    let edited = dir.join("edited.json");
    std::fs::copy(&finalized, &edited).unwrap();
    edit_json(&edited, |genesis| {
        genesis["alloc"][BOB]["balance"] = json!("0x1");
    });

    // A prefill a bad archive stopped after the first archive.
    let stopped = fixture_copy();
    break_second_archive(stopped.path());
    let chain = stopped.path().join("genesis.json");
    try_prefill(&chain, stopped.path(), &[]).expect_err("the bad archive is refused");

    // Every archive in, `--finalize` not run.
    let unfinalized = fixture_copy();
    prefill(unfinalized.path(), &[]);

    // Finalized from another archive set: one attribute in the second archive.
    let other = fixture_copy();
    let part = other.path().join("A1/entities-0001.jsonl");
    let changed = std::fs::read_to_string(&part)
        .unwrap()
        .replace("str:long-lived", "str:other");
    std::fs::write(&part, changed).unwrap();
    prefill(other.path(), &[]);
    prefill(other.path(), &["--finalize"]);

    // The store of a chain that was not prefilled: a node ran on the genesis
    // file without the field.
    let plain = dir.join("plain");
    let mut node = NodeBuilder::new(BIN)
        .chain(&before_finalize)
        .golemdb_path(&plain)
        .spawn();
    node.wait_ready(&connect_reader(&node.http_url()), READY)
        .await;
    node.terminate();

    let cases = [
        (
            &finalized,
            &empty,
            "the Arkiv store is empty. Run `arkiv-reth prefill` into this store first",
        ),
        (
            &finalized,
            &stopped.path().join("golemdb"),
            "the Arkiv store holds an unfinished prefill: 1 of 2 archives are in",
        ),
        (
            &finalized,
            &unfinalized.path().join("golemdb"),
            "the Arkiv store holds a finished prefill whose block 0 is not finalized yet",
        ),
        (
            &edited,
            &store,
            "the genesis file changed after the prefill",
        ),
        (
            &before_finalize,
            &store,
            "the Arkiv store was prefilled, but the genesis file has no \
             config.arkiv.prefilledBlockZeroStateRoot",
        ),
        (
            &finalized,
            &other.path().join("golemdb"),
            "the Arkiv store's prefill was finalized as another block 0 than this genesis file's",
        ),
        (&finalized, &plain, "the Arkiv store holds another chain"),
    ];
    for (chain, store, message) in cases {
        let log = refusal(chain, store).await;
        assert!(
            log.contains(message),
            "{} on {}: expected {message:?} in\n{log}",
            chain.display(),
            store.display()
        );
    }

    // None of them was written to: the right pair still starts.
    let mut node = NodeBuilder::new(BIN)
        .chain(&finalized)
        .golemdb_path(&store)
        .spawn();
    let client = connect_reader(&node.http_url());
    node.wait_ready(&client, READY).await;
    assert_eq!(block_hash(&client, 0).await, expected().1);
}

/// A prefill stopped by a bad archive resumes, once the archive is fixed,
/// after the archive already in, and reaches the fixture's block 0: the same
/// as a run that never stopped.
#[test]
fn a_prefill_stopped_by_a_bad_archive_resumes_to_the_same_block_zero() {
    let work = fixture_copy();
    let dir = work.path();
    let good = break_second_archive(dir);
    let chain = dir.join("genesis.json");
    let error = try_prefill(&chain, dir, &[]).expect_err("the bad archive is refused");
    // The first archive is in; the error names the bad line.
    assert!(error.contains("committed as commit 2"), "{error}");
    assert!(
        error.contains("A1/entities-0002.jsonl:2: not a JSON object"),
        "{error}"
    );

    std::fs::write(dir.join("A1/entities-0002.jsonl"), good).unwrap();
    let resumed = prefill(dir, &[]);
    assert_eq!(
        resumed.matches("committed as commit").count(),
        1,
        "only the second archive is written:\n{resumed}"
    );
    let finalized = prefill(dir, &["--finalize"]);
    let (root, hash) = expected();
    assert!(
        finalized.contains(&format!("state root {root}, hash {hash}")),
        "{finalized}"
    );
}

/// A finalize that commits block 0 but cannot write the genesis file names
/// the root, and so does running it again. Added to the file by hand, as the
/// runbook says, the root starts the node on the fixture's block 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finalize_that_cannot_write_the_genesis_file_is_finished_by_hand() {
    use std::os::unix::fs::PermissionsExt;

    let work = fixture_copy();
    let dir = work.path();
    prefill(dir, &[]);
    // The genesis file in a read-only directory: it cannot be replaced.
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::copy(dir.join("genesis.json"), locked.join("genesis.json")).unwrap();
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(&locked, mode(0o555)).unwrap();
    let chain = locked.join("genesis.json");
    let failed = try_prefill(&chain, dir, &["--finalize"]);
    let again = try_prefill(&chain, dir, &["--finalize"]);
    std::fs::set_permissions(&locked, mode(0o755)).unwrap();

    let (root, hash) = expected();
    let failed = failed.expect_err("the genesis file's directory is read-only (not root?)");
    assert!(
        failed.contains("block 0 is committed, but writing") && failed.contains(&root.to_string()),
        "{failed}"
    );
    let again = again.expect_err("block 0 is already committed");
    assert!(
        again.contains(&format!("block 0's root is {root}, its hash {hash}")),
        "{again}"
    );

    let by_hand = dir.join("genesis.json");
    edit_json(&by_hand, |genesis| {
        genesis["config"]["arkiv"]["prefilledBlockZeroStateRoot"] = json!(root.to_string());
    });
    let mut node = NodeBuilder::new(BIN)
        .chain(&by_hand)
        .golemdb_path(dir.join("golemdb"))
        .spawn();
    let client = connect_reader(&node.http_url());
    node.wait_ready(&client, READY).await;
    assert_eq!(block_hash(&client, 0).await, hash);
    assert_eq!(client.entity_count(None, None).await, 6);
}
