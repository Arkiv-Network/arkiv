//! A burst of reads fails no read and does not slow the chain.
//!
//! Every read of the store holds one of its MDBX reader slots while it runs, and
//! there are fewer slots than a node has threads to read on. Two things keep a
//! burst harmless. A limit on the requests running at once leaves slots free for
//! block building. And a read that still finds no slot free waits for one; it
//! used to fail, and the node panicked on it.
//!
//! A node gets two bursts, as a read outlives a caller that hangs up: callers
//! that wait for their answers, then callers that hang up first.

use std::time::{Duration, Instant};

use alloy_primitives::Address;
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use arkiv_harness::{ArkivClient, DEV_KEY_0, Node, NodeBuilder, connect};

/// How long to wait for a freshly-spawned node's RPC to answer.
const READY: Duration = Duration::from_secs(90);

/// How often the node makes a block.
const BLOCK_TIME: Duration = Duration::from_millis(250);

/// Callers reading at once: more than the store's 112 reader slots, fewer than
/// the 500 connections reth accepts.
const CALLERS: usize = 400;

/// How long a burst lasts. Without the wait, thousands of reads fail in it.
const BURST: Duration = Duration::from_secs(5);

/// How long a caller of the second burst waits before it hangs up: long enough
/// for the node to start the read, too short for it to answer under the burst.
const HANG_UP_AFTER: Duration = Duration::from_micros(1500);

/// What a caller does once it has asked.
#[derive(Clone, Copy)]
enum Caller {
    Waits,
    HangsUp,
}

/// [`CALLERS`] tasks ask for `account`'s balance over and over for [`BURST`].
/// Returns the first read that failed, if any.
async fn burst(
    provider: &(impl Provider + Clone + 'static),
    account: Address,
    caller: Caller,
) -> Option<String> {
    let deadline = Instant::now() + BURST;
    let callers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let provider = provider.clone();
            tokio::spawn(async move {
                while Instant::now() < deadline {
                    let read = provider.get_balance(account).into_future();
                    let error = match caller {
                        Caller::Waits => read.await.err().map(|error| error.to_string()),
                        // Hanging up breaks connections, which is no read's fault:
                        // only an error the node answered with counts.
                        Caller::HangsUp => tokio::time::timeout(HANG_UP_AFTER, read)
                            .await
                            .ok()
                            .and_then(Result::err)
                            .and_then(|error| Some(error.as_error_resp()?.to_string())),
                    };
                    if error.is_some() {
                        return error;
                    }
                }
                None
            })
        })
        .collect();
    let mut first_error = None;
    for caller in callers {
        first_error = first_error.or(caller.await.expect("a caller panicked"));
    }
    first_error
}

/// The two bursts, by name.
const BURSTS: [(Caller, &str); 2] = [
    (Caller::Waits, "callers that wait"),
    (Caller::HangsUp, "callers that hang up"),
];

/// `node` on GolemDB, making a block every [`BLOCK_TIME`] and answering, with a
/// client for it and the account the bursts read.
async fn ready(node: NodeBuilder) -> (Node, ArkivClient<impl Provider + Clone + use<>>, Address) {
    let mut node = node
        .golemdb_store()
        .block_time(format!("{}ms", BLOCK_TIME.as_millis()))
        .spawn();
    let signer: PrivateKeySigner = DEV_KEY_0.parse().unwrap();
    let account = signer.address();
    let client = connect(&node.http_url(), signer);
    node.wait_ready(&client, READY).await;
    (node, client, account)
}

fn node() -> NodeBuilder {
    NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
}

/// How many blocks the node makes while `work` runs, and what `work` returns.
async fn blocks_during<T>(
    client: &ArkivClient<impl Provider>,
    work: impl Future<Output = T>,
) -> (u64, T) {
    let first_block = client.block_number().await;
    let done = work.await;
    (client.block_number().await - first_block, done)
}

/// A read whose caller hung up fails unseen, except in the node's log.
fn assert_no_read_lacked_a_slot(node: &Node) {
    let log = std::fs::read_to_string(node.log_path()).expect("the node's log");
    for failure in ["ReadersFull", "panicked"] {
        assert!(
            !log.contains(failure),
            "the node's log has `{failure}`: a read found no reader slot",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_burst_of_reads_fails_none_and_keeps_the_block_rate_on_golemdb() {
    let (node, client, account) = ready(node()).await;
    // What the node makes left alone for as long as a burst lasts. That depends
    // on how fast its disk commits, so the bursts are measured against it.
    let (idle_blocks, ()) = blocks_during(&client, tokio::time::sleep(BURST)).await;

    for (caller, name) in BURSTS {
        let reads = burst(client.provider(), account, caller);
        let (blocks, first_error) = blocks_during(&client, reads).await;
        assert_eq!(first_error, None, "{name}: a read failed");
        // Three quarters leaves room for a busy machine. Without the request
        // limit, a node whose store commits quickly makes under half.
        assert!(
            blocks * 4 >= idle_blocks * 3,
            "{name}: {blocks} blocks where the idle node made {idle_blocks}",
        );
    }
    assert_no_read_lacked_a_slot(&node);
}

/// The request limit keeps reads below the slot count, so here it is raised out
/// of the way. IPC requests, which it does not cover, can read as much.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn reads_past_the_reader_slots_wait_for_one_on_golemdb() {
    let unlimited = node()
        .arg("--rpc.max-concurrent-requests")
        .arg(CALLERS.to_string());
    let (node, client, account) = ready(unlimited).await;

    for (caller, name) in BURSTS {
        let first_error = burst(client.provider(), account, caller).await;
        assert_eq!(first_error, None, "{name}: a read failed");
    }
    assert_no_read_lacked_a_slot(&node);
}
