//! One load run: plan, send from many senders at once, watch every block, and
//! write the report.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_network::{Ethereum, EthereumWallet, NetworkWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::IEntityRegistry;
use arkiv_harness::{
    ARKIV_ADDRESS, ArkivClient, DEV_CHAIN_ID, NodeBuilder, PoolHashes, connect, connect_reader,
};
use serde_json::{Value, json};

use crate::ledger::{Inclusion, Ledger, Record, Violation};
use crate::model::{Model, mismatch};
use crate::plan::{Body, Expect, PlannedTx, SINK, TRANSFER_GAS, plan_sender};
use crate::report::ReportFile;
use crate::rng::Rng;

/// Well above the dev chain's 1 gwei floor, so a base fee that climbs under
/// load never strands a transaction.
const GAS_PRICE: u128 = 10_000_000_000;
/// The dev chain funds 20 keys; the last is the self-check's phantom sender.
const PHANTOM_SENDER: usize = 19;
const MAX_SENDERS: usize = PHANTOM_SENDER;
/// How often the watcher compares the ledger with the pool.
const POOL_CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// Pool checks in a row that must miss a transaction before it counts as lost:
/// one miss can be a transaction leaving the pool for a block the watcher has
/// not read yet.
const LOST_AFTER_MISSES: u32 = 2;
/// How long the watcher waits, once every sender is done, for the last
/// transactions to land.
const END_GRACE: Duration = Duration::from_secs(60);
/// How long a back-to-back sender waits for its previous transaction.
const BACK_TO_BACK_WAIT: Duration = Duration::from_secs(30);
/// Entities read back per block, at most; all of them at the end.
const ENTITY_CHECKS_PER_BLOCK: usize = 16;
/// Blocks kept for the report.
const RECENT_BLOCKS: usize = 20;
const READY: Duration = Duration::from_secs(90);
/// The node's stdout, which is its log file: the pool's and the payload
/// builder's account of every transaction, including why one was dropped or
/// skipped, which they log at trace.
const NODE_LOG_FILTER: &str =
    "info,txpool=trace,payload_builder=trace,arkiv=debug,arkiv-reth=debug";

pub struct Settings {
    pub seed: u64,
    pub txs: usize,
    pub senders: usize,
    /// Record one transaction as sent without sending it: the run must then
    /// fail with a `lost` violation naming it.
    pub phantom: bool,
}

impl Settings {
    /// From `ARKIV_LOAD_SEED`, `ARKIV_LOAD_TXS` and `ARKIV_LOAD_SENDERS`, with
    /// a fresh seed, 300 transactions and 16 senders by default.
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.parse().unwrap_or_else(|e| panic!("{name}={v}: {e}")))
        };
        Self {
            seed: var("ARKIV_LOAD_SEED").unwrap_or_else(|| unix_time().as_nanos() as u64),
            txs: var("ARKIV_LOAD_TXS").map_or(300, |n: u64| n as usize),
            senders: var("ARKIV_LOAD_SENDERS").map_or(16, |n: u64| n as usize),
            phantom: false,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "txs": self.txs,
            "senders": self.senders,
            "phantom": self.phantom,
            "store": "memory",
            "gasPrice": GAS_PRICE.to_string(),
            "nodeLogFilter": NODE_LOG_FILTER,
        })
    }
}

/// Where a run left its report, and what the report says.
pub struct RunResult {
    pub dir: PathBuf,
    pub report: Value,
}

impl RunResult {
    pub fn outcome(&self) -> &str {
        self.report["outcome"].as_str().unwrap_or_default()
    }
}

/// What the senders and the watcher share.
#[derive(Default)]
struct Shared {
    ledger: Ledger,
    violations: Vec<Violation>,
    /// Set at the first violation: the senders stop sending.
    stop: bool,
    senders_done: usize,
    tip: u64,
}

type SharedRef = Arc<Mutex<Shared>>;

fn unix_time() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
}

pub async fn run(settings: Settings) -> RunResult {
    assert!(
        (1..=MAX_SENDERS).contains(&settings.senders),
        "1 to {MAX_SENDERS} senders"
    );
    let started = Instant::now();
    let dir = std::env::temp_dir().join(format!(
        "arkiv-load-{}-{}-{}",
        settings.seed,
        unix_time().as_secs(),
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut file = ReportFile::new(dir.join("report.json"));
    let header = json!({
        "outcome": "running",
        "seed": settings.seed.to_string(),
        "settings": settings.to_json(),
        "startedAt": unix_time().as_secs(),
        "nodeLog": "node.log",
        "transactionsFile": "transactions.jsonl",
    });
    file.write(&header);
    // Printed both ways: stdout for `--no-capture`, stderr for a failure's
    // captured output.
    let banner = format!(
        "load run: seed {}, {} txs from {} senders, report {}",
        settings.seed,
        settings.txs,
        settings.senders,
        file.path().display()
    );
    println!("{banner}");
    eprintln!("{banner}");

    let mut node = NodeBuilder::new(env!("CARGO_BIN_EXE_arkiv-reth"))
        .log_file(dir.join("node.log"))
        .arg("--log.stdout.filter")
        .arg(NODE_LOG_FILTER)
        // Plain text: the log goes into a report someone reads, not a terminal.
        .arg("--color")
        .arg("never")
        .spawn();
    let watcher = connect_reader(&node.http_url());
    node.wait_ready(&watcher, READY).await;

    let signers = arkiv_genesis::dev_signers(PHANTOM_SENDER + 1).unwrap();
    let shared: SharedRef = Arc::default();
    let mut plans = Vec::new();
    {
        let mut s = shared.lock().unwrap();
        for (sender, signer) in signers.iter().enumerate().take(settings.senders) {
            let address = signer.address();
            let count = settings.txs / settings.senders
                + usize::from(sender < settings.txs % settings.senders);
            let ids: Vec<usize> = plan_sender(settings.seed, sender as u64, address, count)
                .into_iter()
                .map(|plan| {
                    s.ledger.push(Record {
                        sender,
                        sender_address: address,
                        plan,
                        nonce: None,
                        hash: None,
                        sent: None,
                        rejected: None,
                        seen_in_pool_ms: None,
                        included: None,
                        misses: 0,
                        phantom: false,
                    })
                })
                .collect();
            plans.push(ids);
        }
    }
    let plan_digest = {
        let s = shared.lock().unwrap();
        plan_digest(&s.ledger.records)
    };
    let mut header = header;
    header["planDigest"] = plan_digest.into();
    file.write(&header);
    if settings.phantom {
        record_phantom(&shared, &signers[PHANTOM_SENDER], started).await;
    }

    let mut tasks = Vec::new();
    for (sender, ids) in plans.into_iter().enumerate() {
        let client = connect(&node.http_url(), signers[sender].clone());
        let address = signers[sender].address();
        let back_to_back = sender % 4 == 3;
        let rng = Rng::new(settings.seed, 1_000 + sender as u64);
        tasks.push(tokio::spawn(send_all(
            client,
            address,
            Arc::clone(&shared),
            ids,
            back_to_back,
            rng,
            started,
        )));
    }

    let watched = watch(&watcher, &shared, settings.senders, started).await;
    for task in tasks {
        task.await.expect("a sender task panicked");
    }
    let report = final_report(header, &shared, watched, started);
    write_transactions(&dir, &shared);
    file.finish(&report);
    node.terminate();
    RunResult { dir, report }
}

/// Every transaction of the run, one JSON object per line, in sender and
/// nonce order: what it asked for, where it ended up, and what it paid.
/// Written before the final report, so a finished report has it beside it.
fn write_transactions(dir: &std::path::Path, shared: &SharedRef) {
    let s = shared.lock().unwrap();
    let mut lines = String::new();
    for record in &s.ledger.records {
        lines.push_str(&record.to_json().to_string());
        lines.push('\n');
    }
    let path = dir.join("transactions.jsonl");
    std::fs::write(&path, lines).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Sign a transfer from the phantom sender and record it as sent, without
/// sending it.
async fn record_phantom(shared: &SharedRef, signer: &PrivateKeySigner, started: Instant) {
    let address = signer.address();
    let request = TransactionRequest::default()
        .with_from(address)
        .with_to(SINK)
        .with_value(U256::from(1))
        .with_nonce(0)
        .with_gas_limit(TRANSFER_GAS)
        .with_gas_price(GAS_PRICE)
        .with_chain_id(DEV_CHAIN_ID);
    let envelope =
        NetworkWallet::<Ethereum>::sign_request(&EthereumWallet::from(signer.clone()), request)
            .await
            .unwrap();
    let mut s = shared.lock().unwrap();
    let id = s.ledger.push(Record {
        sender: PHANTOM_SENDER,
        sender_address: address,
        plan: PlannedTx {
            label: "phantom: signed, recorded, never sent".into(),
            body: Body::Transfer {
                value: U256::from(1),
            },
            gas: TRANSFER_GAS,
            expect: Expect::Success,
            effects: Vec::new(),
        },
        nonce: None,
        hash: None,
        sent: None,
        rejected: None,
        seen_in_pool_ms: None,
        included: None,
        misses: 0,
        phantom: true,
    });
    s.ledger.sent(id, 0, *envelope.tx_hash(), 0, ms(started));
}

/// A fingerprint of every planned transaction, in sender and nonce order: two
/// runs with the same seed and sizes plan the same, whatever their timing.
fn plan_digest(records: &[Record]) -> String {
    // FNV-1a over each plan's debug form, which spells out every field.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for record in records {
        let text = format!("{}:{:?}", record.sender, record.plan);
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

fn ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// Send one sender's transactions in order, with explicit nonces: in bursts,
/// or each right after the previous one landed.
async fn send_all<P: Provider + Clone + Send + Sync + 'static>(
    client: ArkivClient<P>,
    address: Address,
    shared: SharedRef,
    ids: Vec<usize>,
    back_to_back: bool,
    mut rng: Rng,
    started: Instant,
) {
    let mut nonce = 0;
    let mut previous: Option<usize> = None;
    for id in ids {
        if back_to_back && let Some(previous) = previous {
            let deadline = Instant::now() + BACK_TO_BACK_WAIT;
            loop {
                let (landed, stop) = {
                    let s = shared.lock().unwrap();
                    (s.ledger.records[previous].included.is_some(), s.stop)
                };
                if landed || stop || Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let plan = {
            let s = shared.lock().unwrap();
            if s.stop {
                break;
            }
            s.ledger.records[id].plan.clone()
        };
        let result = send(client.provider(), address, &plan, nonce).await;
        let carry_on = {
            let mut s = shared.lock().unwrap();
            let tip = s.tip;
            match (result, plan.expect) {
                (Ok(hash), Expect::RejectedAtIngress) => {
                    s.ledger.sent(id, nonce, hash, tip, ms(started));
                    s.violations.push(Violation {
                        kind: "unexpected_outcome",
                        block: None,
                        txs: vec![hash],
                        keys: Vec::new(),
                        detail: format!("the node accepted `{}`", plan.label),
                    });
                    s.stop = true;
                    false
                }
                (Ok(hash), _) => {
                    s.ledger.sent(id, nonce, hash, tip, ms(started));
                    nonce += 1;
                    previous = Some(id);
                    true
                }
                (Err(e), Expect::RejectedAtIngress) => {
                    s.ledger.records[id].rejected = Some(e);
                    true
                }
                (Err(e), _) => {
                    s.ledger.records[id].rejected = Some(e.clone());
                    let sender = s.ledger.records[id].sender;
                    s.violations.push(Violation {
                        kind: "unexpected_outcome",
                        block: None,
                        txs: Vec::new(),
                        keys: Vec::new(),
                        detail: format!(
                            "the node refused `{}` from sender {sender} at nonce {nonce}: {e}",
                            plan.label
                        ),
                    });
                    s.stop = true;
                    false
                }
            }
        };
        if !carry_on {
            break;
        }
        if !back_to_back {
            let pause = if rng.chance(10) {
                rng.range(100, 400)
            } else {
                rng.range(0, 20)
            };
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
    }
    shared.lock().unwrap().senders_done += 1;
}

/// Submit one planned transaction, returning its hash or the node's refusal.
async fn send<P: Provider>(
    provider: &P,
    address: Address,
    plan: &PlannedTx,
    nonce: u64,
) -> Result<B256, String> {
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, provider);
    let sent = match &plan.body {
        Body::Execute(ops) => {
            registry
                .execute(ops.clone())
                .nonce(nonce)
                .gas(plan.gas)
                .gas_price(GAS_PRICE)
                .send()
                .await
        }
        Body::EntityNonce => {
            registry
                .entityNonce(address)
                .nonce(nonce)
                .gas(plan.gas)
                .gas_price(GAS_PRICE)
                .send()
                .await
        }
        Body::Transfer { value } => {
            let tx = TransactionRequest::default()
                .with_to(SINK)
                .with_value(*value)
                .with_nonce(nonce)
                .with_gas_limit(plan.gas)
                .with_gas_price(GAS_PRICE)
                .with_chain_id(DEV_CHAIN_ID);
            return provider
                .send_transaction(tx)
                .await
                .map(|pending| *pending.tx_hash())
                .map_err(|e| e.to_string());
        }
    };
    sent.map(|pending| *pending.tx_hash())
        .map_err(|e| e.to_string())
}

/// What the watcher saw, beyond the ledger.
#[derive(Default)]
struct Watched {
    first_block: Option<u64>,
    last_block: u64,
    recent_blocks: VecDeque<Value>,
    /// Transactions in the run's blocks that the run did not send: the
    /// sequencer's own, such as its purge calls.
    foreign_txs: Vec<Value>,
    pool: PoolHashes,
    entities_tracked: usize,
    entity_checks: usize,
    model_vs_actual: Vec<Value>,
}

/// Read every block as it comes, check the invariants, and return once the run
/// is over: at the first violation, or once every transaction is accounted for.
async fn watch(
    client: &ArkivClient<impl Provider>,
    shared: &SharedRef,
    senders: usize,
    started: Instant,
) -> Watched {
    let mut watched = Watched::default();
    let mut model = Model::default();
    let mut next_nonce: HashMap<usize, u64> = HashMap::new();
    let mut next_block = 1;
    let mut last_pool_check = Instant::now();
    let mut drained_at: Option<Instant> = None;
    loop {
        read_blocks(
            client,
            shared,
            &mut next_block,
            &mut model,
            &mut next_nonce,
            &mut watched,
        )
        .await;
        if shared.lock().unwrap().stop {
            break;
        }

        if last_pool_check.elapsed() >= POOL_CHECK_INTERVAL {
            last_pool_check = Instant::now();
            let snapshot_ms = ms(started);
            if let Ok(pool) = client.pool_hashes().await {
                // Blocks mined while the snapshot was taken hold transactions
                // that left the pool for them.
                read_blocks(
                    client,
                    shared,
                    &mut next_block,
                    &mut model,
                    &mut next_nonce,
                    &mut watched,
                )
                .await;
                check_pool(shared, &pool, snapshot_ms, next_block - 1);
                watched.pool = pool;
            }
            if shared.lock().unwrap().stop {
                break;
            }
        }

        let (done, outstanding) = {
            let s = shared.lock().unwrap();
            let outstanding: Vec<B256> = s
                .ledger
                .records
                .iter()
                .filter(|r| r.is_outstanding())
                .filter_map(|r| r.hash)
                .collect();
            (s.senders_done == senders, outstanding)
        };
        if done && outstanding.is_empty() {
            break;
        }
        if done {
            let since = *drained_at.get_or_insert_with(Instant::now);
            if since.elapsed() > END_GRACE {
                let mut s = shared.lock().unwrap();
                s.violations.push(Violation {
                    kind: "unclean_end",
                    block: Some(next_block - 1),
                    txs: outstanding,
                    keys: Vec::new(),
                    detail: format!(
                        "every sender finished, and these were still not in a block \
                         {END_GRACE:?} later"
                    ),
                });
                s.stop = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if shared.lock().unwrap().violations.is_empty() {
        final_checks(client, shared, &model, &mut watched).await;
    }
    watched.entities_tracked = model.len();
    watched
}

/// Read blocks `next_block..=tip`, folding each into the ledger and the model.
async fn read_blocks(
    client: &ArkivClient<impl Provider>,
    shared: &SharedRef,
    next_block: &mut u64,
    model: &mut Model,
    next_nonce: &mut HashMap<usize, u64>,
    watched: &mut Watched,
) {
    let tip = client.block_number().await;
    shared.lock().unwrap().tip = tip;
    while *next_block <= tip {
        let number = *next_block;
        let Some(hashes) = client.block_tx_hashes(number).await else {
            return;
        };
        let receipts: HashMap<B256, (bool, u64, u128)> = client
            .provider()
            .get_block_receipts(number.into())
            .await
            .expect("eth_getBlockReceipts")
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                (
                    r.transaction_hash,
                    (r.status(), r.gas_used, r.effective_gas_price),
                )
            })
            .collect();
        watched.first_block.get_or_insert(number);
        watched.last_block = number;
        watched.recent_blocks.push_back(json!({
            "number": number,
            "txs": hashes.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>(),
        }));
        if watched.recent_blocks.len() > RECENT_BLOCKS {
            watched.recent_blocks.pop_front();
        }

        let foreign: Vec<B256> = {
            let s = shared.lock().unwrap();
            hashes
                .iter()
                .filter(|h| s.ledger.by_hash(h).is_none())
                .copied()
                .collect()
        };
        for hash in foreign {
            foreign_tx(client, shared, number, hash, watched).await;
        }

        let mut touched = BTreeSet::new();
        {
            let mut s = shared.lock().unwrap();
            for (index, hash) in hashes.iter().enumerate() {
                let Some(id) = s.ledger.by_hash(hash) else {
                    continue;
                };
                let (status, gas_used, effective_gas_price) =
                    receipts.get(hash).copied().unwrap_or((false, 0, 0));
                let record = s.ledger.records[id].clone();
                if let Some(earlier) = &record.included {
                    s.violations.push(Violation {
                        kind: "duplicate",
                        block: Some(number),
                        txs: vec![*hash],
                        keys: Vec::new(),
                        detail: format!("already included in block {}", earlier.block),
                    });
                    continue;
                }
                s.ledger.records[id].included = Some(Inclusion {
                    block: number,
                    index,
                    status,
                    gas_used,
                    effective_gas_price,
                });
                let expected_nonce = next_nonce.entry(record.sender).or_insert(0);
                if record.nonce != Some(*expected_nonce) {
                    s.violations.push(Violation {
                        kind: "nonce_gap",
                        block: Some(number),
                        txs: vec![*hash],
                        keys: Vec::new(),
                        detail: format!(
                            "sender {} landed nonce {:?}, its next was {expected_nonce}",
                            record.sender, record.nonce
                        ),
                    });
                }
                *expected_nonce = record.nonce.unwrap_or(0) + 1;
                let as_expected = match record.plan.expect {
                    Expect::Success => status,
                    Expect::Revert => !status,
                    Expect::OutOfGas => !status && gas_used == record.plan.gas,
                    Expect::RejectedAtIngress => false,
                };
                if !as_expected {
                    s.violations.push(Violation {
                        kind: "unexpected_outcome",
                        block: Some(number),
                        txs: vec![*hash],
                        keys: record.plan.effects.iter().map(|e| e.key()).collect(),
                        detail: format!(
                            "`{}` expected {}, got {} using {gas_used} of {} gas",
                            record.plan.label,
                            record.plan.expect.name(),
                            if status { "success" } else { "a revert" },
                            record.plan.gas,
                        ),
                    });
                } else if record.plan.expect == Expect::Success {
                    touched.extend(model.apply(
                        record.sender_address,
                        number,
                        &record.plan.effects,
                    ));
                }
            }
            if !s.violations.is_empty() {
                s.stop = true;
            }
        }

        for key in touched.into_iter().take(ENTITY_CHECKS_PER_BLOCK) {
            let actual = client.get_entity_at(key, number).await;
            watched.entity_checks += 1;
            check_entity(shared, model, key, &actual, Some(number), watched);
        }
        *next_block += 1;
    }
}

/// Note a transaction the run did not send. One from a run sender is a
/// finding: the run sends every transaction of its senders and records each.
async fn foreign_tx(
    client: &ArkivClient<impl Provider>,
    shared: &SharedRef,
    block: u64,
    hash: B256,
    watched: &mut Watched,
) {
    let tx = client
        .provider()
        .raw_request::<_, Value>("eth_getTransactionByHash".into(), (hash,))
        .await
        .unwrap_or(Value::Null);
    let from: Option<Address> = tx["from"].as_str().and_then(|f| f.parse().ok());
    let input = tx["input"].as_str().unwrap_or_default();
    watched.foreign_txs.push(json!({
        "hash": format!("{hash:#x}"),
        "block": block,
        "from": tx["from"],
        "to": tx["to"],
        "selector": input.get(..10).unwrap_or(input),
    }));
    let mut s = shared.lock().unwrap();
    if let Some(from) = from
        && s.ledger.records.iter().any(|r| r.sender_address == from)
    {
        s.violations.push(Violation {
            kind: "unknown_tx",
            block: Some(block),
            txs: vec![hash],
            keys: Vec::new(),
            detail: format!("from run sender {from:#x}, but not a transaction the run sent"),
        });
        s.stop = true;
    }
}

fn check_entity(
    shared: &SharedRef,
    model: &Model,
    key: B256,
    actual: &Value,
    block: Option<u64>,
    watched: &mut Watched,
) {
    let Some(expected) = model.get(&key) else {
        return;
    };
    if let Some(difference) = mismatch(expected, actual) {
        watched.model_vs_actual.push(json!({
            "key": format!("{key:#x}"),
            "block": block,
            "expected": expected.as_ref().map(|e| e.to_json()),
            "actual": actual,
        }));
        let mut s = shared.lock().unwrap();
        s.violations.push(Violation {
            kind: "entity_mismatch",
            block,
            txs: Vec::new(),
            keys: vec![key],
            detail: difference,
        });
        s.stop = true;
    }
}

/// Compare the ledger with a pool snapshot taken at `snapshot_ms`, after
/// reading every block up to `read_to`.
fn check_pool(shared: &SharedRef, pool: &PoolHashes, snapshot_ms: u64, read_to: u64) {
    let mut s = shared.lock().unwrap();
    let mut lost = Vec::new();
    for record in s.ledger.records.iter_mut() {
        let (Some(hash), Some((_, sent_ms))) = (record.hash, record.sent) else {
            continue;
        };
        // Sent after the snapshot was asked for: the snapshot cannot hold it.
        if record.included.is_some() || sent_ms >= snapshot_ms {
            continue;
        }
        if pool.contains(&hash) {
            record.seen_in_pool_ms.get_or_insert(snapshot_ms);
            record.misses = 0;
        } else {
            record.misses += 1;
            if record.misses >= LOST_AFTER_MISSES {
                lost.push(hash);
            }
        }
    }
    if !lost.is_empty() {
        s.violations.push(Violation {
            kind: "lost",
            block: Some(read_to),
            txs: lost,
            keys: Vec::new(),
            detail: format!(
                "accepted by the node, but neither in the pool nor in any block up to \
                 {read_to}, in {LOST_AFTER_MISSES} pool checks in a row"
            ),
        });
        s.stop = true;
    }
}

/// Once everything landed: nothing of the run is left in the pool, and every
/// entity the model knows reads back as the model says at the tip.
async fn final_checks(
    client: &ArkivClient<impl Provider>,
    shared: &SharedRef,
    model: &Model,
    watched: &mut Watched,
) {
    if let Ok(pool) = client.pool_hashes().await {
        let leftover: Vec<B256> = {
            let s = shared.lock().unwrap();
            s.ledger
                .records
                .iter()
                .filter_map(|r| r.hash)
                .filter(|h| pool.contains(h))
                .collect()
        };
        if !leftover.is_empty() {
            let mut s = shared.lock().unwrap();
            s.violations.push(Violation {
                kind: "unclean_end",
                block: None,
                txs: leftover,
                keys: Vec::new(),
                detail: "still in the pool after landing".into(),
            });
        }
        watched.pool = pool;
    }
    let keys: Vec<B256> = model.keys().copied().collect();
    for key in keys {
        let actual = client.get_entity(key).await;
        watched.entity_checks += 1;
        check_entity(shared, model, key, &actual, None, watched);
    }
}

fn final_report(
    mut report: Value,
    shared: &SharedRef,
    watched: Watched,
    started: Instant,
) -> Value {
    let s = shared.lock().unwrap();
    let records: Vec<&Record> = s.ledger.records.iter().filter(|r| !r.phantom).collect();
    let count = |kind: &str| s.violations.iter().filter(|v| v.kind == kind).count();
    let txs_in = |kind: &str| -> usize {
        s.violations
            .iter()
            .filter(|v| v.kind == kind)
            .map(|v| v.txs.len())
            .sum()
    };
    let included = records.iter().filter(|r| r.included.is_some()).count();
    let duration_ms = started.elapsed().as_millis() as u64;
    let mut expect_counts = BTreeMap::new();
    for record in &records {
        *expect_counts.entry(record.plan.expect.name()).or_insert(0) += 1;
    }
    let mut foreign_kinds = BTreeMap::new();
    for tx in &watched.foreign_txs {
        let kind = format!(
            "from {} to {} selector {}",
            tx["from"].as_str().unwrap_or("?"),
            tx["to"].as_str().unwrap_or("?"),
            tx["selector"].as_str().unwrap_or("?")
        );
        *foreign_kinds.entry(kind).or_insert(0) += 1;
    }
    let failed = !s.violations.is_empty();
    report["outcome"] = if failed { "failed" } else { "passed" }.into();
    report["finishedAt"] = unix_time().as_secs().into();
    report["summary"] = json!({
        "planned": records.len(),
        "plannedByExpectation": expect_counts,
        "sent": records.iter().filter(|r| r.hash.is_some()).count(),
        "rejectedAtIngress": records.iter().filter(|r| r.rejected.is_some()).count(),
        "included": included,
        "succeeded": records.iter().filter(|r| r.included.as_ref().is_some_and(|i| i.status)).count(),
        "reverted": records.iter().filter(|r| r.included.as_ref().is_some_and(|i| !i.status)).count(),
        "outstanding": s.ledger.records.iter().filter(|r| r.is_outstanding()).count(),
        "lost": txs_in("lost"),
        "duplicated": count("duplicate"),
        "nonceGaps": count("nonce_gap"),
        "unexpectedOutcomes": count("unexpected_outcome"),
        "entityMismatches": count("entity_mismatch"),
        "foreignTxs": watched.foreign_txs.len(),
        "unknownTxs": count("unknown_tx"),
        "foreignTxKinds": foreign_kinds,
        "blocks": { "first": watched.first_block, "last": watched.last_block },
        "entitiesTracked": watched.entities_tracked,
        "entityChecks": watched.entity_checks,
        "durationMs": duration_ms,
        "includedPerSecond": included as f64 * 1000.0 / duration_ms.max(1) as f64,
    });
    report["violations"] = s.violations.iter().map(Violation::to_json).collect();
    if failed {
        report["transactions"] = s.ledger.records.iter().map(Record::to_json).collect();
        report["recentBlocks"] = watched.recent_blocks.into_iter().collect();
        report["pool"] = json!({
            "pending": watched.pool.pending.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>(),
            "queued": watched.pool.queued.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>(),
        });
        report["foreignTxs"] = watched.foreign_txs.clone().into();
        report["modelVsActual"] = watched.model_vs_actual.into();
    }
    report
}
