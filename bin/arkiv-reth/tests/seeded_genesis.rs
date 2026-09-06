//! Seeded genesis: a node whose block 0 already holds thousands of entities,
//! built offline by `arkiv-seed` and handed to reth as genesis state.
//!
//! Black-box over the real binary, two ways in: a genesis file with the seeded
//! `alloc` (`--chain <file>`), and a `reth init-state` dump imported behind a
//! genesis carrying `stateHash`. Both must serve the seed from block 0, keep
//! minting nonces continuous for the seeded owners, and — because the pruning
//! map learns about genesis entities from a bootstrap walk rather than from
//! logs — purge seeded entities once they expire.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, AttributeValue, IEntityRegistry, Ident32, Operation};
use arkiv_harness::{
    ARKIV_ADDRESS, ArkivClient, DEV_CHAIN_ID, DEV_KEY_0, DEV_KEY_1, EntityCreationNonce,
    NodeBuilder, connect, derive_entity_address, hex_quantity, result_keys,
};
use arkiv_seed::{SeedManifest, SeedSpec, StreamSink, export};

/// How long to wait for a freshly-spawned node's RPC to answer (debug reth is
/// slow, and a seeded genesis has thousands of accounts to hash first).
const READY: Duration = Duration::from_secs(120);

fn dev_address(key: &str) -> Address {
    key.parse::<PrivateKeySigner>().unwrap().address()
}

/// A scratch path unique to this process and moment.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "arkiv-seed-e2e-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// `count` entities of `payload_size` bytes, dealt to the two dev keys,
/// expiring at `expires_at`.
fn seed_spec(count: u64, payload_size: usize, expires_at: u64) -> SeedSpec {
    let mut spec = SeedSpec::new(
        DEV_CHAIN_ID,
        vec![dev_address(DEV_KEY_0), dev_address(DEV_KEY_1)],
    );
    spec.count = count;
    spec.payload_size = payload_size;
    spec.expires_at = expires_at;
    spec
}

/// The spec's entities plus the dev funding — the whole block-0 state, in
/// memory.
fn seed(count: u64, payload_size: usize, expires_at: u64) -> (SeedSpec, arkiv_seed::SeededState) {
    let spec = seed_spec(count, payload_size, expires_at);
    let state = arkiv_seed::build_in_memory(&spec, arkiv_genesis::genesis_alloc().unwrap(), |_| {})
        .expect("seed builds");
    (spec, state)
}

/// The seed as a genesis file with the state in `alloc`.
fn seeded_genesis_file(
    count: u64,
    payload_size: usize,
    expires_at: u64,
) -> (PathBuf, SeedManifest) {
    let (_, state) = seed(count, payload_size, expires_at);
    let genesis = export::genesis_with_alloc(export::dev_genesis(DEV_CHAIN_ID), state.alloc);
    let path = scratch("genesis.json");
    std::fs::write(&path, serde_json::to_string(&genesis).unwrap()).expect("write genesis");
    (path, state.manifest)
}

/// A create carrying `payload` under `text/plain`, with a purely relative
/// lifetime.
fn create_op(min_lifetime: u64, payload: Bytes) -> Operation {
    let attrs = vec![
        Attribute::from_value(
            Ident32::system("$contentType").unwrap(),
            &AttributeValue::Str("text/plain".into()),
        )
        .unwrap(),
        Attribute::from_value(
            Ident32::system("$payload").unwrap(),
            &AttributeValue::Bytes(payload.to_vec()),
        )
        .unwrap(),
    ];
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

/// The key the seed minted for entity `i`, derived the way a client would.
fn seeded_key(spec: &SeedSpec, i: u64) -> B256 {
    B256::from(derive_entity_address(
        spec.chain_id,
        &spec.owner_of(i).into_array(),
        spec.nonce_of(i),
        0,
    ))
}

/// Block 0's header as the node serves it: its hash and state root are what
/// two nodes seeded from one file must agree on.
async fn genesis_header(client: &ArkivClient<impl Provider>) -> alloy_rpc_types::Header {
    client
        .provider()
        .get_block_by_number(0.into())
        .await
        .unwrap()
        .expect("genesis block")
        .header
}

async fn wait_until_purged(client: &ArkivClient<impl Provider>, keys: &[B256]) {
    let deadline = Instant::now() + Duration::from_secs(60);
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
            "{remaining} seeded entities were not physically purged within 60 seconds"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Two thousand 1 KiB entities in the genesis file: counted, readable, indexed
/// and queryable from block 0, with the seeded owners' minting nonces carried
/// on so a post-genesis create mints the next key — and ordinary ops on seeded
/// entities behave like on any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_genesis_serves_entities_from_block_zero() {
    const COUNT: u64 = 2000;
    const PAYLOAD: usize = 1024;
    let (genesis, manifest) = seeded_genesis_file(COUNT, PAYLOAD, 1_000_000);
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis)
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let owner0 = signer.address();
    let owner1 = dev_address(DEV_KEY_1);
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    let _ = std::fs::remove_file(&genesis);
    assert_eq!(client.chain_id().await, DEV_CHAIN_ID);

    // The whole seed is live at genesis and still at the tip.
    assert_eq!(client.entity_count(None, Some(0)).await, COUNT);
    assert_eq!(client.entity_count(None, None).await, COUNT);

    // Entity 0 belongs to the first owner, entity 1 to the second; both read
    // back with the seed's fields.
    let key0 = manifest.sample_keys[0];
    let entity = client.get_entity(key0).await;
    assert!(!entity.is_null(), "seeded entity must exist");
    assert_eq!(entity["key"], format!("{key0:#x}"));
    assert_eq!(
        entity["owner"].as_str().unwrap().to_lowercase(),
        format!("{owner0:#x}")
    );
    assert_eq!(
        entity["creator"].as_str().unwrap().to_lowercase(),
        format!("{owner0:#x}")
    );
    assert_eq!(hex_quantity(&entity["createdAt"]), 0);
    assert_eq!(hex_quantity(&entity["expiresAt"]), 1_000_000);
    assert_eq!(entity["contentType"], "application/octet-stream");
    let payload = entity["payload"].as_str().unwrap();
    assert_eq!(payload.len(), 2 + 2 * PAYLOAD, "1 KiB payload, hex-encoded");
    assert!(
        payload.starts_with("0x61726b69762d73656564"),
        "payload starts with the `arkiv-seed` header"
    );
    assert_eq!(
        entity["attributes"],
        serde_json::json!([
            { "name": "rank", "type": "u256", "value": "0x0" },
            { "name": "team", "type": "str", "value": "red" },
        ])
    );
    let second = client.get_entity(manifest.sample_keys[1]).await;
    assert_eq!(
        second["owner"].as_str().unwrap().to_lowercase(),
        format!("{owner1:#x}")
    );

    // The index answers every query class over the seed, at the tip and at
    // block 0. rank = i mod 100; team cycles red, green, blue.
    assert_eq!(client.entity_count(Some("rank = u256(7)"), None).await, 20);
    assert_eq!(
        client.entity_count(Some("rank >= u256(90)"), None).await,
        200
    );
    assert_eq!(
        client.entity_count(Some("team = str('red')"), None).await,
        667
    );
    assert_eq!(
        client
            .entity_count(Some("team STARTSWITH str('gr')"), None)
            .await,
        667
    );
    assert_eq!(
        client.entity_count(Some("team = str('blue')"), None).await,
        666
    );
    assert_eq!(
        client
            .entity_count(Some(&format!("$owner = addr({owner0:#x})")), None)
            .await,
        1000
    );
    assert_eq!(
        client
            .entity_count(Some("rank = u256(7) AND team = str('red')"), Some(0))
            .await,
        6,
        "rank 7 is every 7+100k; red is every third ordinal, so k = 2, 5, ..., 17"
    );
    let page = client.query("rank = u256(7)", 100, None).await;
    let keys = result_keys(&page);
    assert_eq!(keys.len(), 20);
    assert!(keys.contains(&format!("{:#x}", manifest.sample_keys[7])));
    assert_eq!(
        hex_quantity(&page["blockNumber"]),
        client.block_number().await
    );

    // Minting nonces continue where the seed left off: each owner got 1000.
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, client.provider());
    assert_eq!(registry.entityNonce(owner0).call().await.unwrap(), 1000);
    assert_eq!(registry.entityNonce(owner1).call().await.unwrap(), 1000);
    assert_eq!(manifest.owner_nonces[&owner0], 1000);
    client
        .execute(vec![create_op(1000, Bytes::from_static(b"after genesis"))])
        .await;
    let next_key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &owner0.into_array(),
        EntityCreationNonce::new(1000),
        0,
    ));
    assert!(
        !client.get_entity(next_key).await.is_null(),
        "the first post-genesis create mints nonce 1000's key"
    );
    assert_eq!(client.entity_count(None, None).await, COUNT + 1);

    // Seeded entities take ordinary ops from their owner, and refuse a stranger.
    client
        .execute(vec![patch_payload(key0, Bytes::from_static(b"patched"))])
        .await;
    let patched = client.get_entity(key0).await;
    assert_eq!(patched["payload"], "0x70617463686564");
    assert!(hex_quantity(&patched["updatedAt"]) > 0);
    let key2 = manifest.sample_keys[2];
    client.execute(vec![Operation::delete(key2)]).await;
    assert!(client.get_entity(key2).await.is_null());
    assert_eq!(client.entity_count(None, None).await, COUNT);
    let stranger = connect(&node.http_url(), DEV_KEY_1.parse().unwrap());
    assert!(
        !stranger
            .try_execute(vec![patch_payload(key0, Bytes::from_static(b"x"))])
            .await,
        "a non-owner's patch of a seeded entity reverts"
    );
    // Block 0 still answers the original seed.
    assert_eq!(client.entity_count(None, Some(0)).await, COUNT);
    assert!(!client.get_entity_at(key2, 0).await.is_null());
}

/// Seeded entities expire like any other: hidden from reads at their expiry
/// block, then physically purged by the protocol's per-block purge — which
/// only knows about them because the pruning map bootstraps from genesis.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_entities_expire_and_are_purged() {
    const COUNT: u64 = 25;
    const EXPIRES_AT: u64 = 15;
    let (spec, state) = seed(COUNT, 64, EXPIRES_AT);
    let genesis = export::genesis_with_alloc(export::dev_genesis(DEV_CHAIN_ID), state.alloc);
    let path = scratch("expiring-genesis.json");
    std::fs::write(&path, serde_json::to_string(&genesis).unwrap()).unwrap();
    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&path)
        .spawn();
    let client = arkiv_harness::connect_reader(&node.http_url());
    node.wait_ready(&client, READY).await;
    let _ = std::fs::remove_file(&path);

    let keys: Vec<B256> = (0..COUNT).map(|i| seeded_key(&spec, i)).collect();
    assert_eq!(client.entity_count(None, Some(0)).await, COUNT);

    client.wait_for_block(EXPIRES_AT, READY).await;
    assert_eq!(
        client.entity_count(None, None).await,
        0,
        "at the expiry block every seeded entity is logically gone"
    );
    assert!(client.get_entity(keys[0]).await.is_null());
    // Block 0 still has them.
    assert_eq!(client.entity_count(None, Some(0)).await, COUNT);

    // The purge runs ten keys a block; all 25 go within a few blocks.
    wait_until_purged(&client, &keys).await;
}

/// The unlimited route: the seed as a `reth init-state` dump behind a genesis
/// that names its state root, imported before the node starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_state_imports_a_seeded_dump_at_genesis() {
    const COUNT: u64 = 300;
    let spec = seed_spec(COUNT, 512, 1_000_000);
    // The dump streams out of the builder, as it would for a seed too big to
    // hold: the root in its first line is the one the genesis names.
    let dump = scratch("state.jsonl");
    let mut sink = StreamSink::create(&dump, scratch("state.sort")).unwrap();
    let manifest = arkiv_seed::build(
        &spec,
        arkiv_genesis::genesis_alloc().unwrap(),
        &mut sink,
        |_| {},
    )
    .expect("seed streams");
    let genesis =
        export::genesis_with_state_hash(export::dev_genesis(DEV_CHAIN_ID), manifest.state_root)
            .unwrap();
    let genesis_path = scratch("state-hash-genesis.json");
    std::fs::write(&genesis_path, genesis.to_string()).unwrap();

    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis_path)
        .init_state(&dump)
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let owner0 = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    let _ = std::fs::remove_file(&dump);
    let _ = std::fs::remove_file(&genesis_path);

    // Block 0 carries the seed's root, and the state behind it.
    assert_eq!(
        genesis_header(&client).await.state_root,
        manifest.state_root
    );
    assert_eq!(client.entity_count(None, Some(0)).await, COUNT);
    assert_eq!(client.entity_count(None, None).await, COUNT);
    let entity = client.get_entity(seeded_key(&spec, 0)).await;
    assert_eq!(entity["payload"].as_str().unwrap().len(), 2 + 2 * 512);
    assert_eq!(
        client.entity_count(Some("team = str('green')"), None).await,
        100
    );

    // The chain runs on it: blocks advance, and the seeded owner's next create
    // continues its nonce sequence (150 each for two owners).
    let tip = client.block_number().await;
    client.wait_for_block(tip + 2, READY).await;
    client
        .execute(vec![create_op(
            1000,
            Bytes::from_static(b"on imported state"),
        )])
        .await;
    let next_key = B256::from(derive_entity_address(
        DEV_CHAIN_ID,
        &owner0.into_array(),
        EntityCreationNonce::new(150),
        0,
    ));
    assert!(!client.get_entity(next_key).await.is_null());
    assert_eq!(client.entity_count(None, None).await, COUNT + 1);
}

/// One genesis file, two nodes: the same genesis hash, so a sequencer and a
/// follower seeded from the same file share a chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_genesis_is_identical_across_nodes() {
    let (genesis, manifest) = seeded_genesis_file(50, 256, 1_000_000);
    let mut a = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis)
        .spawn();
    let mut b = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .chain(&genesis)
        .spawn();
    let client_a = arkiv_harness::connect_reader(&a.http_url());
    let client_b = arkiv_harness::connect_reader(&b.http_url());
    a.wait_ready(&client_a, READY).await;
    b.wait_ready(&client_b, READY).await;
    let _ = std::fs::remove_file(&genesis);

    let header_a = genesis_header(&client_a).await;
    let header_b = genesis_header(&client_b).await;
    assert_eq!(header_a.hash, header_b.hash);
    assert_eq!(header_a.state_root, manifest.state_root);
    assert_eq!(client_a.entity_count(None, Some(0)).await, 50);
    assert_eq!(client_b.entity_count(None, Some(0)).await, 50);
}
