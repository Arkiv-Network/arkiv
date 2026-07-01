//! Continuous load generator for the Arkiv precompile.
//!
//! Maintains an in-memory pool of "alive" entities and submits a weighted
//! random mix of CREATE/UPDATE/EXTEND/TRANSFER/DELETE operations against
//! a running node. Past-expiry entities are picked up by EXPIRE
//! preferentially, so the alive pool stays bounded and EXPIRE coverage
//! happens naturally.
//!
//! ## Concurrency model
//!
//! Each signer is a "slot" with a busy flag. Up to `signer_count` batches
//! can be in flight at once — one per signer, so each account's nonce
//! stream stays sequential without manual tracking. The driver loop ticks
//! at `1/rate` seconds, picks an idle slot, builds a batch from the
//! current state, and spawns a submission task. The task awaits the
//! receipt and applies per-op state transitions under a shared mutex.
//!
//! ## Multi-op batches
//!
//! Each tx contains `1..=max_ops_per_tx` operations. Op selection within
//! a batch is independent — no in-batch cross-references, so we don't
//! need to predict entity keys ahead of time. (Real-life users do batch
//! create-then-update sometimes; the simulator's mix gets us most of the
//! coverage without that complexity.)

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, SolEvent};
use arkiv_bindings::{
    IEntityRegistry, IEntityRegistry::EntityOperation, Mime128, OP_CREATE, Operation,
};
use arkiv_genesis::dev_signers;
use clap::Args;
use eyre::{Result, bail};
use rand::{Rng, SeedableRng, seq::IndexedRandom};
use rand_chacha::ChaCha8Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct SimulateArgs {
    /// Batches per second. With multi-op tx and per-signer concurrency,
    /// effective ops/s is roughly `rate * average_batch_size`.
    #[arg(long, default_value_t = 0.5)]
    pub rate: f64,

    /// Total runtime; "0" or "infinity" means run until Ctrl-C.
    #[arg(long, default_value = "0", value_parser = parse_duration_or_zero)]
    pub duration: Duration,

    /// Number of mnemonic-derived signers to rotate through. Each signer
    /// can have at most one in-flight tx, so this also caps in-flight
    /// concurrency. Capped at `arkiv_genesis::ARKIV_DEV_ACCOUNT_COUNT`.
    #[arg(long, default_value_t = 10)]
    pub signer_count: usize,

    /// Max operations per transaction. Each batch contains `1..=N` ops
    /// drawn from the weighted mix. Set to 1 to mirror the old single-op
    /// behaviour.
    #[arg(long, default_value_t = 5)]
    pub max_ops_per_tx: usize,

    /// Op weights, e.g. "create=4,update=3,extend=2,transfer=1,delete=1".
    /// EXPIRE is event-driven (fires when an entity passes expiry) and
    /// has no weight.
    #[arg(long, default_value = "create=4,update=3,extend=2,transfer=1,delete=1")]
    pub weights: String,

    /// Cap on simultaneously-tracked alive entities. CREATE is throttled
    /// when this is reached.
    #[arg(long, default_value_t = 1000)]
    pub max_alive: usize,

    /// Status report interval.
    #[arg(long, default_value = "10s", value_parser = humantime::parse_duration)]
    pub status_interval: Duration,

    /// Deterministic RNG seed. Omit for non-reproducible runs.
    #[arg(long)]
    pub seed: Option<u64>,
}

/// Parse a duration string, treating "0" / "infinity" / "inf" as a sentinel
/// for "unbounded" — represented by [`Duration::ZERO`] in the parsed value.
fn parse_duration_or_zero(s: &str) -> std::result::Result<Duration, String> {
    if s == "0" || s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("inf") {
        Ok(Duration::ZERO)
    } else {
        humantime::parse_duration(s).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Op kinds + weight parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
enum OpKind {
    Create,
    Update,
    Extend,
    Transfer,
    Delete,
    Expire,
}

impl OpKind {
    fn name(self) -> &'static str {
        match self {
            OpKind::Create => "create",
            OpKind::Update => "update",
            OpKind::Extend => "extend",
            OpKind::Transfer => "transfer",
            OpKind::Delete => "delete",
            OpKind::Expire => "expire",
        }
    }

    fn from_name(s: &str) -> Result<Self> {
        Ok(match s {
            "create" => OpKind::Create,
            "update" => OpKind::Update,
            "extend" => OpKind::Extend,
            "transfer" => OpKind::Transfer,
            "delete" => OpKind::Delete,
            other => bail!("unknown op kind: {}", other),
        })
    }
}

fn parse_weights(s: &str) -> Result<Vec<(OpKind, u32)>> {
    let mut out = Vec::new();
    for entry in s.split(',') {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("expected key=value, got '{}'", entry))?;
        let kind = OpKind::from_name(k.trim())?;
        let weight: u32 = v
            .trim()
            .parse()
            .map_err(|e| eyre::eyre!("invalid weight '{}': {}", v, e))?;
        out.push((kind, weight));
    }
    if out.iter().all(|(_, w)| *w == 0) {
        bail!("all op weights are zero");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AliveEntity {
    owner_idx: usize,
    expires_at: u32,
    /// True while a tx referencing this entity is in flight; excludes it
    /// from selection by other parallel batches.
    pending: bool,
}

#[derive(Default)]
struct State {
    alive: HashMap<B256, AliveEntity>,
    expired: Vec<B256>,
    /// EXPIRE candidates that have been reserved by an in-flight batch;
    /// returned to `expired` on failure.
    expired_pending: Vec<B256>,
    counts: HashMap<OpKind, Counters>,
}

#[derive(Default, Debug, Clone, Copy)]
struct Counters {
    sent: u64,
    confirmed: u64,
    failed: u64,
}

impl State {
    fn record_sent(&mut self, op: OpKind) {
        self.counts.entry(op).or_default().sent += 1;
    }
    fn record_confirmed(&mut self, op: OpKind) {
        self.counts.entry(op).or_default().confirmed += 1;
    }
    fn record_failed(&mut self, op: OpKind) {
        self.counts.entry(op).or_default().failed += 1;
    }

    /// Move alive entries past `current_block` into the expired queue,
    /// skipping entities currently pending (a TRANSFER/EXTEND in flight
    /// might still mutate them; let it complete before EXPIRE-ing).
    fn promote_expired(&mut self, current_block: u32) {
        let mut to_expire = Vec::new();
        for (k, e) in &self.alive {
            if !e.pending && e.expires_at <= current_block {
                to_expire.push(*k);
            }
        }
        for k in to_expire {
            self.alive.remove(&k);
            self.expired.push(k);
        }
    }

    fn alive_count(&self) -> usize {
        self.alive.len()
    }
}

// ---------------------------------------------------------------------------
// Batch building
// ---------------------------------------------------------------------------

/// One operation prepared for inclusion in an outgoing batch, plus enough
/// context to apply the post-receipt state transition.
#[derive(Debug)]
struct PlannedOp {
    kind: OpKind,
    op: Operation,
    /// CREATE: `None` (we learn the key from the receipt logs).
    /// All others: the targeted entity key.
    target: Option<B256>,
    /// CREATE: the expiry block we asked for; copied into the alive entry
    /// once we recover the key.
    new_expires_at: Option<u32>,
    /// TRANSFER: index of the new owner in the signer pool.
    new_owner_idx: Option<usize>,
}

/// A complete batch ready to submit on behalf of one signer.
#[derive(Debug)]
struct BatchPlan {
    signer_idx: usize,
    ops: Vec<PlannedOp>,
}

/// Build a multi-op batch for `signer_idx`. Returns `None` if no feasible
/// op exists (e.g. nothing alive, signer owns nothing, weights all
/// excluded). All entity targets are marked `pending=true` in `state`.
#[allow(clippy::too_many_arguments)]
fn build_batch(
    signer_idx: usize,
    state: &mut State,
    weights: &[(OpKind, u32)],
    rng: &mut ChaCha8Rng,
    max_alive: usize,
    max_ops_per_tx: usize,
    signer_count: usize,
    current_block: u64,
) -> Option<BatchPlan> {
    // Per-batch counters of CREATEs we've already added — needed so the
    // CREATE feasibility check accounts for entities not yet committed.
    let mut creates_in_batch = 0usize;

    // Each batch is exactly one tx, so reserve up to max_ops_per_tx slots.
    let target_size = rng.random_range(1..=max_ops_per_tx.max(1));
    let mut ops: Vec<PlannedOp> = Vec::with_capacity(target_size);

    while ops.len() < target_size {
        let kind = pick_op_kind(
            state,
            signer_idx,
            weights,
            rng,
            max_alive,
            signer_count,
            creates_in_batch,
        )?;
        let planned = build_op(kind, signer_idx, state, rng, signer_count, current_block);
        match planned {
            Some(p) => {
                if p.kind == OpKind::Create {
                    creates_in_batch += 1;
                }
                ops.push(p);
            }
            None => break, // no feasible op of this kind; stop building
        }
    }

    if ops.is_empty() {
        return None;
    }

    // Mark every targeted entity as pending so other parallel batches skip it.
    for op in &ops {
        if let Some(key) = op.target
            && let Some(e) = state.alive.get_mut(&key)
        {
            e.pending = true;
        }
    }
    for op in &ops {
        state.record_sent(op.kind);
    }

    Some(BatchPlan { signer_idx, ops })
}

#[allow(clippy::too_many_arguments)]
fn pick_op_kind(
    state: &State,
    signer_idx: usize,
    weights: &[(OpKind, u32)],
    rng: &mut ChaCha8Rng,
    max_alive: usize,
    signer_count: usize,
    creates_in_batch: usize,
) -> Option<OpKind> {
    // Prefer EXPIRE if any past-expiry entity is queued (and not already
    // claimed by another in-flight batch).
    if !state.expired.is_empty() {
        return Some(OpKind::Expire);
    }

    let feasible: Vec<(OpKind, u32)> = weights
        .iter()
        .copied()
        .filter(|(k, w)| {
            *w > 0
                && is_feasible(
                    *k,
                    state,
                    signer_idx,
                    max_alive,
                    signer_count,
                    creates_in_batch,
                )
        })
        .collect();
    if feasible.is_empty() {
        return None;
    }

    let total: u32 = feasible.iter().map(|(_, w)| *w).sum();
    let mut roll = rng.random_range(0..total);
    for (kind, weight) in feasible {
        if roll < weight {
            return Some(kind);
        }
        roll -= weight;
    }
    None
}

fn is_feasible(
    kind: OpKind,
    state: &State,
    signer_idx: usize,
    max_alive: usize,
    signer_count: usize,
    creates_in_batch: usize,
) -> bool {
    let owned_available = || {
        state
            .alive
            .values()
            .any(|e| e.owner_idx == signer_idx && !e.pending)
    };
    match kind {
        OpKind::Create => state.alive_count() + creates_in_batch < max_alive,
        OpKind::Update | OpKind::Extend | OpKind::Delete => owned_available(),
        OpKind::Transfer => owned_available() && signer_count >= 2,
        OpKind::Expire => !state.expired.is_empty(),
    }
}

/// Build a `PlannedOp` of the given kind. Marks the target entity (if any)
/// as pending and removes from `expired` if EXPIRE.
fn build_op(
    kind: OpKind,
    signer_idx: usize,
    state: &mut State,
    rng: &mut ChaCha8Rng,
    signer_count: usize,
    current_block: u64,
) -> Option<PlannedOp> {
    match kind {
        OpKind::Create => {
            let lifespan = rng.random_range(30u64..300);
            let expires_at = (current_block + lifespan) as u32;
            let size = rng.random_range(64..512);
            let payload = random_payload(rng, size);
            Some(PlannedOp {
                kind,
                op: Operation::create(lifespan as u32, payload, random_content_type(rng), vec![]),
                target: None,
                new_expires_at: Some(expires_at),
                new_owner_idx: None,
            })
        }
        OpKind::Update => {
            let key = pick_owned_alive(state, signer_idx, rng)?;
            let size = rng.random_range(64..512);
            let payload = random_payload(rng, size);
            Some(PlannedOp {
                kind,
                op: Operation::update(key, payload, random_content_type(rng), vec![]),
                target: Some(key),
                new_expires_at: None,
                new_owner_idx: None,
            })
        }
        OpKind::Extend => {
            let key = pick_owned_alive(state, signer_idx, rng)?;
            let entity = state.alive.get(&key)?.clone();
            let bump = rng.random_range(50u64..400);
            let new_expires_at = (current_block + bump).max(entity.expires_at as u64 + 1) as u32;
            let btl = (new_expires_at as u64 - current_block) as u32;
            Some(PlannedOp {
                kind,
                op: Operation::extend(key, btl),
                target: Some(key),
                new_expires_at: Some(new_expires_at),
                new_owner_idx: None,
            })
        }
        OpKind::Transfer => {
            let key = pick_owned_alive(state, signer_idx, rng)?;
            // Choose a different signer.
            let mut new_idx = rng.random_range(0..signer_count);
            if new_idx == signer_idx {
                new_idx = (new_idx + 1) % signer_count;
            }
            // Target address resolved later in submit_plan from the wallet.
            // Stash the index here so we know what to update on receipt.
            Some(PlannedOp {
                kind,
                // newOwner is Address::ZERO here; submit_plan overwrites it from the signer pool.
                op: Operation::transfer(key, Address::ZERO),
                target: Some(key),
                new_expires_at: None,
                new_owner_idx: Some(new_idx),
            })
        }
        OpKind::Delete => {
            let key = pick_owned_alive(state, signer_idx, rng)?;
            Some(PlannedOp {
                kind,
                op: Operation::delete(key),
                target: Some(key),
                new_expires_at: None,
                new_owner_idx: None,
            })
        }
        OpKind::Expire => {
            let key = state.expired.pop()?;
            state.expired_pending.push(key);
            Some(PlannedOp {
                kind,
                op: Operation::expire(key),
                target: None, // no `alive` entry to flip pending on
                new_expires_at: None,
                new_owner_idx: None,
            })
        }
    }
}

fn pick_owned_alive(state: &State, signer_idx: usize, rng: &mut ChaCha8Rng) -> Option<B256> {
    let candidates: Vec<B256> = state
        .alive
        .iter()
        .filter(|(_, e)| e.owner_idx == signer_idx && !e.pending)
        .map(|(k, _)| *k)
        .collect();
    candidates.choose(rng).copied()
}

fn random_payload(rng: &mut ChaCha8Rng, size: usize) -> Bytes {
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    Bytes::from(buf)
}

fn random_content_type(rng: &mut ChaCha8Rng) -> Mime128 {
    const TYPES: &[&str] = &[
        "application/json",
        "application/octet-stream",
        "text/plain",
        "image/png",
        "image/jpeg",
    ];
    let s = TYPES.choose(rng).copied().unwrap_or(TYPES[0]);
    Mime128::encode(s).expect("hardcoded MIME types are always valid")
}

// ---------------------------------------------------------------------------
// Submission
// ---------------------------------------------------------------------------

/// Submit a planned batch and apply state transitions on the resulting
/// receipt. Always clears `pending` flags and the busy slot before
/// returning. All counters are updated in-place on `state`.
#[allow(clippy::too_many_arguments)]
async fn submit_plan<P: Provider + Clone + Send + Sync + 'static>(
    plan: BatchPlan,
    state: Arc<Mutex<State>>,
    provider: P,
    registry: Address,
    signer_addrs: Arc<[Address]>,
    gas_price: u128,
    busy: Arc<AtomicBool>,
) {
    let _guard = SlotGuard { busy };

    // Fill in TRANSFER newOwner addresses now that we have the signer pool.
    let mut ops_with_addresses = plan.ops;
    for planned in &mut ops_with_addresses {
        if planned.kind == OpKind::Transfer {
            let idx = planned.new_owner_idx.expect("transfer plan");
            planned.op.newOwner = signer_addrs[idx];
        }
    }

    let calldata = IEntityRegistry::executeCall {
        ops: ops_with_addresses.iter().map(|p| p.op.clone()).collect(),
    }
    .abi_encode();
    let tx = TransactionRequest::default()
        .with_from(signer_addrs[plan.signer_idx])
        .with_to(registry)
        .with_input(calldata)
        .with_gas_price(gas_price);

    let result = async {
        let pending = provider.send_transaction(tx).await?;
        let receipt = pending.get_receipt().await?;
        Ok::<_, eyre::Report>(receipt)
    }
    .await;

    let mut state = state.lock().await;
    match result {
        Ok(receipt) if receipt.status() => {
            apply_success(
                &mut state,
                &ops_with_addresses,
                receipt.inner.logs(),
                plan.signer_idx,
            );
        }
        Ok(receipt) => {
            tracing::warn!(tx = %receipt.transaction_hash, "tx reverted");
            apply_failure(&mut state, &ops_with_addresses);
        }
        Err(e) => {
            tracing::warn!(error = %e, "submission failed");
            apply_failure(&mut state, &ops_with_addresses);
        }
    }
}

/// RAII guard that clears the slot's busy flag when dropped, ensuring
/// the slot is released even if the submission task panics.
struct SlotGuard {
    busy: Arc<AtomicBool>,
}
impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

fn apply_success(
    state: &mut State,
    planned: &[PlannedOp],
    logs: &[alloy_rpc_types::eth::Log],
    sender_idx: usize,
) {
    // Walk EntityOperation logs in order; CREATE ops consume one each.
    let mut create_keys: Vec<B256> = logs
        .iter()
        .filter_map(|log| EntityOperation::decode_log(&log.inner).ok())
        .filter(|ev| ev.data.operationType == OP_CREATE)
        .map(|ev| ev.data.entityKey)
        .collect();
    create_keys.reverse(); // pop from end → consume in original order

    for op in planned {
        match op.kind {
            OpKind::Create => {
                if let Some(key) = create_keys.pop() {
                    state.alive.insert(
                        key,
                        AliveEntity {
                            owner_idx: sender_idx,
                            expires_at: op.new_expires_at.unwrap_or(0),
                            pending: false,
                        },
                    );
                }
            }
            OpKind::Update => {
                if let Some(key) = op.target
                    && let Some(e) = state.alive.get_mut(&key)
                {
                    e.pending = false;
                }
            }
            OpKind::Extend => {
                if let Some(key) = op.target
                    && let Some(e) = state.alive.get_mut(&key)
                {
                    e.expires_at = op.new_expires_at.unwrap_or(e.expires_at);
                    e.pending = false;
                }
            }
            OpKind::Transfer => {
                if let Some(key) = op.target
                    && let Some(new_idx) = op.new_owner_idx
                    && let Some(e) = state.alive.get_mut(&key)
                {
                    e.owner_idx = new_idx;
                    e.pending = false;
                }
            }
            OpKind::Delete | OpKind::Expire => {
                if let Some(key) = op.target {
                    state.alive.remove(&key);
                } else {
                    // EXPIRE: the key is in the op itself.
                    let key = op.op.entityKey;
                    let pos = state.expired_pending.iter().position(|k| *k == key);
                    if let Some(pos) = pos {
                        state.expired_pending.remove(pos);
                    }
                }
            }
        }
        state.record_confirmed(op.kind);
    }
}

fn apply_failure(state: &mut State, planned: &[PlannedOp]) {
    for op in planned {
        // Restore pending flags / expired queue.
        if let Some(key) = op.target
            && let Some(e) = state.alive.get_mut(&key)
        {
            e.pending = false;
        }
        if op.kind == OpKind::Expire {
            let key = op.op.entityKey;
            let pos = state.expired_pending.iter().position(|k| *k == key);
            if let Some(pos) = pos {
                state.expired_pending.remove(pos);
                state.expired.push(key);
            }
        }
        state.record_failed(op.kind);
    }
}

// ---------------------------------------------------------------------------
// Status reporting
// ---------------------------------------------------------------------------

fn print_status(
    state: &State,
    started: Instant,
    current_block: u64,
    in_flight: usize,
    header: &str,
) {
    let elapsed = started.elapsed();
    println!(
        "{header}  elapsed={elapsed:?}  block={current_block}  alive={}  expired_queue={}  in_flight={}",
        state.alive_count(),
        state.expired.len(),
        in_flight,
    );
    let order = [
        OpKind::Create,
        OpKind::Update,
        OpKind::Extend,
        OpKind::Transfer,
        OpKind::Delete,
        OpKind::Expire,
    ];
    for kind in order {
        let c = state.counts.get(&kind).copied().unwrap_or_default();
        if c.sent == 0 {
            continue;
        }
        println!(
            "  {:<8} sent={:<6} confirmed={:<6} failed={}",
            kind.name(),
            c.sent,
            c.confirmed,
            c.failed
        );
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

pub async fn run(
    args: SimulateArgs,
    rpc_url: &str,
    registry: Address,
    gas_price: u128,
    _block_time: Duration,
) -> Result<()> {
    if args.rate <= 0.0 {
        bail!("--rate must be positive");
    }
    if args.max_ops_per_tx == 0 {
        bail!("--max-ops-per-tx must be at least 1");
    }

    let signers = dev_signers(args.signer_count)?;
    let signer_addrs: Arc<[Address]> = signers.iter().map(|s| s.address()).collect();
    let weights = parse_weights(&args.weights)?;

    let mut wallet = EthereumWallet::from(signers[0].clone());
    for s in &signers[1..] {
        wallet.register_signer(s.clone());
    }
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);

    // One busy flag per signer slot.
    let busy_flags: Arc<[Arc<AtomicBool>]> = (0..signers.len())
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();

    let state = Arc::new(Mutex::new(State::default()));
    let rng = Arc::new(Mutex::new(match args.seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::from_seed(rand::random()),
    }));

    let started = Instant::now();
    let mut last_status = started;
    let tick = Duration::from_secs_f64(1.0 / args.rate);

    println!(
        "simulate: rate={}/s signers={} max_ops_per_tx={} weights=[{}] max_alive={} duration={} seed={}",
        args.rate,
        signers.len(),
        args.max_ops_per_tx,
        args.weights,
        args.max_alive,
        if args.duration.is_zero() {
            "unbounded".to_string()
        } else {
            format!("{:?}", args.duration)
        },
        args.seed
            .map(|s| s.to_string())
            .unwrap_or_else(|| "random".into()),
    );

    let mut interrupted = false;

    loop {
        if !args.duration.is_zero() && started.elapsed() >= args.duration {
            break;
        }

        // Find an idle slot (non-blocking).
        let idle_idx = busy_flags.iter().position(|b| !b.load(Ordering::Acquire));

        if let Some(idx) = idle_idx {
            let current_block = provider.get_block_number().await.unwrap_or(0);
            let plan = {
                let mut state_g = state.lock().await;
                state_g.promote_expired(current_block as u32);
                let mut rng_g = rng.lock().await;
                build_batch(
                    idx,
                    &mut state_g,
                    &weights,
                    &mut rng_g,
                    args.max_alive,
                    args.max_ops_per_tx,
                    signers.len(),
                    current_block,
                )
            };

            if let Some(plan) = plan {
                busy_flags[idx].store(true, Ordering::Release);
                let provider_c = provider.clone();
                let state_c = state.clone();
                let busy_c = busy_flags[idx].clone();
                let addrs_c = signer_addrs.clone();
                tokio::spawn(submit_plan(
                    plan, state_c, provider_c, registry, addrs_c, gas_price, busy_c,
                ));
            }
        }

        if last_status.elapsed() >= args.status_interval {
            let in_flight = busy_flags
                .iter()
                .filter(|b| b.load(Ordering::Acquire))
                .count();
            let s = state.lock().await;
            let block = provider.get_block_number().await.unwrap_or(0);
            print_status(&s, started, block, in_flight, "[status]");
            last_status = Instant::now();
        }

        tokio::select! {
            _ = sleep(tick) => {},
            _ = tokio::signal::ctrl_c() => {
                interrupted = true;
                break;
            }
        }
    }

    // Wait briefly for in-flight tasks to drain.
    let drain_until = Instant::now() + Duration::from_secs(15);
    while busy_flags.iter().any(|b| b.load(Ordering::Acquire)) && Instant::now() < drain_until {
        sleep(Duration::from_millis(100)).await;
    }

    let final_block = provider.get_block_number().await.unwrap_or(0);
    let in_flight = busy_flags
        .iter()
        .filter(|b| b.load(Ordering::Acquire))
        .count();
    let header = if interrupted {
        "[final, interrupted]"
    } else {
        "[final]"
    };
    let s = state.lock().await;
    print_status(&s, started, final_block, in_flight, header);
    Ok(())
}
