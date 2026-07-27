//! Live replication (harness test T2): a follower node replicates a sequencer's
//! chain and arrives at byte-identical state — same block hashes, same entity
//! store, same query index — proving the Arkiv executor is deterministic across
//! nodes.
//!
//! Topology, all black-box over the real binary:
//! - **sequencer** — `arkiv-node --dev` (auto-seals blocks), the only writer.
//! - **follower**  — `arkiv-node` (no `--dev`, so it never seals its own blocks),
//!   dialing the sequencer as a trusted peer.
//! - **CL driver** — the harness [`EngineClient`]: post-merge, devp2p alone will
//!   not sync a follower's live tip, so the harness stands in for a consensus
//!   layer and drives the follower's fork choice to the sequencer's head. reth
//!   then backfills the blocks over P2P and the follower executes them itself.

use std::collections::BTreeSet;
use std::time::Duration;

use alloy_primitives::{B256, Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{Attribute, Ident32, Mime128, Operation};
use arkiv_harness::{
    ARKIV_ADDRESS, DEV_CHAIN_ID, DEV_KEY_0, EngineClient, NodeBuilder, connect, connect_reader,
    derive_entity_key, result_keys, sync_follower, write_jwt_secret,
};

/// Node-boot readiness: up to this many respawns, each given this long to answer
/// RPC (debug reth under parallel node-boot load occasionally wedges on start).
const READY_ATTEMPTS: usize = 3;
const READY_PER_ATTEMPT: Duration = Duration::from_secs(45);
/// How long to wait for the follower to replicate up to the target block.
const REPLICATE: Duration = Duration::from_secs(60);

fn text_plain_mime() -> Mime128 {
    Mime128::encode("text/plain").expect("valid mime")
}

fn attrs(rank: u64, team: &[u8]) -> Vec<Attribute> {
    vec![
        Attribute::uint(Ident32::encode("rank").unwrap(), U256::from(rank)),
        Attribute::string(Ident32::encode("team").unwrap(), team).unwrap(),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follower_replicates_sequencer_state_and_index() {
    let bin = env!("CARGO_BIN_EXE_arkiv-node");
    // A shared JWT secret both nodes read and the CL driver signs with.
    let jwt_path = std::env::temp_dir().join(format!("arkiv-t2-jwt-{}.hex", std::process::id()));
    let jwt = write_jwt_secret(&jwt_path);

    // Sequencer: dev auto-seal. `admin` is enabled so we can read its enode.
    let mut seq_node = NodeBuilder::new(bin)
        .jwt_secret(&jwt_path)
        .http_api("eth,net,web3,txpool,admin")
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let caller = signer.address();
    let sequencer = connect(&seq_node.http_url(), signer);
    sequencer
        .wait_ready_resilient(&mut seq_node, READY_ATTEMPTS, READY_PER_ATTEMPT)
        .await;
    let enode = sequencer.enode().await;

    // Follower: same dev genesis, but NOT `--dev` — it only advances when driven.
    // It dials the sequencer directly since discovery is disabled.
    let mut fol_node = NodeBuilder::new(bin)
        .dev(false)
        .jwt_secret(&jwt_path)
        .trusted_peer(enode)
        .spawn();
    let follower = connect_reader(&fol_node.http_url());
    follower
        .wait_ready_resilient(&mut fol_node, READY_ATTEMPTS, READY_PER_ATTEMPT)
        .await;

    // The CL driver over the follower's authenticated Engine API.
    let engine = EngineClient::new(&fol_node.authrpc_url(), &jwt);

    // Write a batch of entities on the sequencer: state + a query index to replicate.
    let receipt = sequencer
        .execute(vec![
            Operation::create(
                1000,
                Bytes::from_static(b"alpha"),
                text_plain_mime(),
                attrs(10, b"red"),
            ),
            Operation::create(
                1000,
                Bytes::from_static(b"beta"),
                text_plain_mime(),
                attrs(20, b"blue"),
            ),
            Operation::create(
                1000,
                Bytes::from_static(b"gamma"),
                text_plain_mime(),
                attrs(20, b"red"),
            ),
        ])
        .await;
    let keys: Vec<B256> = (0..3)
        .map(|i| B256::from(derive_entity_key(DEV_CHAIN_ID, &caller.into_array(), i)))
        .collect();

    // Replicate up to (at least) the block that included the writes. Compare at
    // that fixed height, since the sequencer keeps sealing empty blocks.
    let target = receipt.block_number.expect("receipt block number");
    sync_follower(&engine, &sequencer, &follower, target, REPLICATE).await;

    // 1) Whole-chain determinism: identical block hash at `target` means every
    //    ancestor — including the state root after applying the entity ops — is
    //    byte-identical on both nodes.
    assert_eq!(
        follower.block_hash_at(target).await,
        sequencer.block_hash_at(target).await,
        "follower and sequencer agree on the block hash at {target}",
    );

    // 2) The follower rebuilt the entity store from the replicated blocks.
    assert_eq!(
        follower.entity_count(None, Some(target)).await,
        sequencer.entity_count(None, Some(target)).await,
        "entity count matches",
    );
    for key in &keys {
        assert_eq!(
            follower.get_entity_at(*key, target).await,
            sequencer.get_entity_at(*key, target).await,
            "entity {key:#x} matches byte-for-byte",
        );
    }

    // 3) The follower rebuilt the query index too: the same filter returns the
    //    same keys on both nodes.
    let team_red: BTreeSet<String> = [keys[0], keys[2]]
        .iter()
        .map(|k| format!("{k:#x}"))
        .collect();
    assert_eq!(
        result_keys(&follower.query_at("team = \"red\"", target).await),
        team_red,
        "the query index replicated (team = red)",
    );
    assert_eq!(
        result_keys(&follower.query_at("rank = 20", target).await),
        result_keys(&sequencer.query_at("rank = 20", target).await),
        "follower and sequencer queries agree (rank = 20)",
    );

    // Sanity: the replicated logs carried the Arkiv entity events (address check
    // keeps ARKIV_ADDRESS meaningful in this test).
    assert_eq!(
        receipt
            .inner
            .logs()
            .iter()
            .filter(|l| l.inner.address == ARKIV_ADDRESS)
            .count(),
        3
    );

    let _ = std::fs::remove_file(&jwt_path);
}
