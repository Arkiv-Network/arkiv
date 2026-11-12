//! Arkiv payload builder: prepend a bounded, protocol-generated purge transaction.

mod chain_pruning_map;

use alloy_consensus::{
    SignableTransaction, Transaction, TxEip1559, TxEnvelope, transaction::SignerRecoverable,
};
use alloy_network::TxSignerSync;
use alloy_primitives::{B256, Bytes, TxKind, U256};
use alloy_rlp::Encodable;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use arkiv_bindings::{
    IEntityRegistry, MAX_PURGE_KEYS, PURGE_CALLER, PURGE_GAS_LIMIT, protocol::purgeExpiredCall,
};
use arkiv_interfaces::store::Store;
use arkiv_reth_executor::ARKIV_ADDRESS;
use arkiv_reth_rpc::store_reads;
use arkiv_reth_statemanager::{
    BlockSeals, ExecutionKey, HostStore, SegmentWrite, segments::rows_for_block,
};
use chain_pruning_map::{ChainPruningMap, PruningEntry};
use futures_util::StreamExt;
use reth_basic_payload_builder::{
    BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig, BuildArguments, BuildOutcome,
    PayloadBuilder,
};
use reth_ethereum::{
    EthPrimitives, TransactionSigned,
    chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks},
    evm::primitives::{ConfigureEvm, NextBlockEnvAttributes},
    node::{
        api::{FullNodeTypes, NodeTypes},
        builder::{BuilderContext, PayloadBuilderConfig, components::PayloadServiceBuilder},
    },
    pool::{
        BestTransactions, EthPooledTransaction, TransactionOrigin, TransactionPool,
        ValidPoolTransaction, error::InvalidPoolTransactionError,
    },
    provider::CanonStateSubscriptions,
};
use reth_ethereum_payload_builder::{
    EthereumBuilderConfig, PayloadStateRootRequest, PayloadStateRootResolver,
    default_ethereum_payload,
};
use reth_node_ethereum::EthEngineTypes;
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderHandle, PayloadBuilderService};
use reth_storage_api::{BlockReader, ReceiptProvider, StateProviderFactory};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

// Distinguishes rebuilds and purge-only fallbacks sharing a payload ID.
static NEXT_BUILD_ATTEMPT: AtomicU64 = AtomicU64::new(1);

/// Public protocol material, not an authentication secret.
const PURGE_ENVELOPE_KEY: &str = "8b3a350cf5c34c9194ca3a545d4b54b69356a5f5a39d9c7f94a17e5f7f9a6c31";

/// Answers a built block's state root from the Arkiv seal that produced it.
///
/// GolemDB computes its own commitment at seal, so the root is a product of
/// execution rather than a walk of an account trie — which is why reth asks for
/// it through a resolver, after execution has ended. See
/// `BlockBuilder::finish_with`.
///
/// The request names the execution by its transactions, not by its height, and
/// the lookup is keyed the same way. Height alone would be ambiguous: reth
/// races a second build against an unfinished one at the same height
/// (`BasicPayloadJob::resolve_kind`), so two seals can be in flight and
/// "whichever sealed last" is the wrong answer for one of them.
///
/// `None` falls back to reth's own state root. That happens when no seal
/// matches — a build that never reached `Evm::finish`, or an empty block, which
/// charges nothing and so seals nothing.
fn arkiv_state_root_resolver(seals: Arc<BlockSeals>) -> PayloadStateRootResolver {
    Arc::new(move |request: &PayloadStateRootRequest<'_>| {
        let key = ExecutionKey::new(
            request.number,
            request
                .transactions
                .iter()
                .map(|(signer, nonce)| (signer.into_array(), *nonce))
                .collect(),
        );
        let root = seals.root_of(&key).map(B256::from);
        if root.is_none() {
            debug!(
                target: "arkiv::payload",
                number = request.number,
                transactions = request.transactions.len(),
                "no Arkiv seal for this execution; reth will compute the state root",
            );
        }
        root
    })
}

#[derive(Debug, Clone)]
pub struct ArkivPayloadServiceBuilder {
    store: HostStore,
    seals: Arc<BlockSeals>,
}

impl ArkivPayloadServiceBuilder {
    /// The payload service over the store Arkiv's state lives in, adopting a
    /// sealed candidate as each block becomes canonical.
    pub const fn new(store: HostStore, seals: Arc<BlockSeals>) -> Self {
        Self { store, seals }
    }
}

impl<Node, Pool, Evm> PayloadServiceBuilder<Node, Pool, Evm> for ArkivPayloadServiceBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<Primitives = EthPrimitives, Payload = EthEngineTypes>>,
    <Node::Types as NodeTypes>::ChainSpec: EthChainSpec + EthereumHardforks,
    Node::Provider: StateProviderFactory
        + BlockReader<Block = reth_ethereum::Block>
        + ReceiptProvider<Receipt = reth_ethereum::Receipt>
        + Clone
        + Unpin
        + 'static,
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Unpin + 'static,
    Evm: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>
        + Send
        + 'static,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm: Evm,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        let pruning_path = ctx.config().datadir().data_dir().join("arkiv-pruning.db");
        let pruning_map = ChainPruningMap::open(&pruning_path, Handle::current()).await?;
        info!(target: "arkiv-reth", path = %pruning_path.display(), "opened chain pruning map");
        let conf = ctx.config().builder.clone();
        // Target gas limit for built blocks: `--builder.gaslimit`, else the genesis `gasLimit`.
        let gas_limit = conf
            .gas_limit
            .unwrap_or(ctx.chain_spec().genesis().gas_limit);
        info!(target: "arkiv-reth", gas_limit, "block gas limit target");
        let payload_builder = ArkivPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm,
            chain_id: ctx.chain_spec().chain().id(),
            pruning_map: pruning_map.clone(),
            store: self.store.clone(),
            seals: self.seals.clone(),
            config: EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_extra_data(conf.extra_data())
                .with_state_root_resolver(arkiv_state_root_resolver(self.seals.clone())),
        };
        let generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            BasicPayloadJobGeneratorConfig::default()
                .interval(conf.interval)
                .deadline(conf.deadline)
                .max_payload_tasks(conf.max_payload_tasks),
            payload_builder,
        );
        spawn_catch_up(
            ctx.task_executor(),
            ctx.provider().clone(),
            self.store.clone(),
            pruning_map.clone(),
        );
        let provider = ctx.provider().clone();
        let store = self.store.clone();
        let seals = self.seals.clone();
        let notifications = Box::pin(ctx.provider().canonical_state_stream().then(
            move |notification| {
                let provider = provider.clone();
                let store = store.clone();
                let seals = seals.clone();
                let pruning_map = pruning_map.clone();
                async move {
                    let started = Instant::now();
                    // The block is canonical, so its Arkiv state is no longer
                    // speculative: promote the seal to a commit, unless the
                    // engine validator saved it already, before a child of it
                    // executed.
                    let tip = notification.tip();
                    let height = tip.number;
                    let block_hash = tip.hash().0;
                    let state_root = tip.state_root.0;
                    debug!(target: "arkiv::timing", height, block_hash = %B256::from(block_hash), "Arkiv canonical notification started");
                    // The block's own rows go into the append-only tier in the
                    // same commit as its state. Built before `adopt` because a
                    // block we cannot encode is one we must not commit state
                    // for either -- the two would disagree permanently.
                    let encoding_started = Instant::now();
                    let writes = match rows_for_canonical_tip(&notification) {
                        Ok(writes) => writes,
                        Err(error) => {
                            warn!(target: "arkiv-reth", %error, height, "failed to encode the block's rows; its Arkiv state is not adopted");
                            seals.finish_adoption(block_hash);
                            info!(target: "arkiv::timing", height, block_hash = %B256::from(block_hash), outcome = "encoding_error", total_ms = started.elapsed().as_secs_f64() * 1000.0, "Arkiv canonical notification finished");
                            return notification;
                        }
                    };
                    let encoding = encoding_started.elapsed();
                    // On a blocking thread, not this one. Adoption is now a
                    // write per transaction and receipt rather than a couple
                    // of calls, and the store's seam is synchronous -- against
                    // an out-of-process store that is thousands of round trips. Blocking this
                    // worker for them stalls the payload builder sharing the
                    // runtime, which costs transactions per block rather than
                    // correctness, and so does not show up in any in-process
                    // test. `catch_up` below already does the same thing.
                    let adopting_store = store.clone();
                    let adopting_seals = seals.clone();
                    let adoption_started = Instant::now();
                    let adopted = tokio::task::spawn_blocking(move || {
                        debug!(target: "arkiv::timing", height, block_hash = %B256::from(block_hash), queue_wait_ms = adoption_started.elapsed().as_secs_f64() * 1000.0, "Arkiv adoption worker started");
                        adopting_seals.adopt(&adopting_store, height, block_hash, state_root, &writes)
                    })
                    .await;
                    let adoption = adoption_started.elapsed();
                    let adoption_outcome = match &adopted {
                        Ok(Ok(Some(_))) => "committed",
                        Ok(Ok(None)) => "no_candidate",
                        Ok(Err(_)) => "error",
                        Err(_) => "worker_error",
                    };
                    match adopted {
                        Ok(Ok(Some(commit))) => {
                            debug!(target: "arkiv-reth", height, commit = commit.0, "adopted the block's Arkiv state")
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(error)) => {
                            warn!(target: "arkiv-reth", ?error, height, "failed to adopt the block's Arkiv state")
                        }
                        Err(error) => {
                            warn!(target: "arkiv-reth", %error, height, "the task adopting the block's Arkiv state did not finish")
                        }
                    }
                    seals.finish_adoption(block_hash);
                    let catch_up_started = Instant::now();
                    let caught_up = catch_up(provider, store, pruning_map).await;
                    let catch_up = catch_up_started.elapsed();
                    info!(
                        target: "arkiv::timing", height, block_hash = %B256::from(block_hash),
                        adoption_outcome, catch_up_ok = caught_up.is_ok(),
                        encoding_ms = encoding.as_secs_f64() * 1000.0,
                        adoption_ms = adoption.as_secs_f64() * 1000.0,
                        catch_up_ms = catch_up.as_secs_f64() * 1000.0,
                        total_ms = started.elapsed().as_secs_f64() * 1000.0,
                        "Arkiv canonical notification finished",
                    );
                    if let Err(error) = caught_up {
                        warn!(target: "arkiv-reth", %error, "failed to advance chain pruning map");
                    }
                    notification
                }
            },
        ));
        let (service, handle) = PayloadBuilderService::new(generator, notifications);
        ctx.task_executor().spawn_critical_os_thread(
            "payload-service",
            "arkiv payload builder service",
            service,
        );
        Ok(handle)
    }
}

/// The rows the canonical tip contributes to the append-only tier.
///
/// # Only the tip
///
/// A notification can commit several blocks at once, and this writes rows for
/// the last of them. That matches `BlockSeals::adopt`, which commits the tip's
/// seal and discards the rest, so rows and state stay consistent with each
/// other — a block with no rows also has no committed state.
///
/// It is still a gap: on a multi-block notification the intermediate blocks
/// reach neither tier. Nothing breaks today because reth keeps its own copy of
/// every block, but that copy is what step 6 switches off, so this has to be
/// closed before then.
fn rows_for_canonical_tip(
    notification: &reth_ethereum::provider::CanonStateNotification,
) -> eyre::Result<Vec<SegmentWrite>> {
    let chain = notification.committed();
    let block = chain.tip();
    let hash = block.hash();
    let receipts = chain
        .receipts_by_block_hash(hash)
        .ok_or_else(|| eyre::eyre!("block {hash} is canonical but has no receipts"))?;
    let receipts: Vec<reth_ethereum::Receipt> = receipts.into_iter().cloned().collect();
    Ok(rows_for_block(
        block.header(),
        &block.body().transactions,
        block.senders(),
        &receipts,
        hash,
    )?)
}

fn spawn_catch_up<P>(
    executor: &reth_ethereum::tasks::TaskExecutor,
    provider: P,
    store: HostStore,
    pruning_map: ChainPruningMap,
) where
    P: StateProviderFactory
        + BlockReader<Block = reth_ethereum::Block>
        + ReceiptProvider<Receipt = reth_ethereum::Receipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    executor.spawn_task(async move {
        if let Err(error) = catch_up(provider, store, pruning_map).await {
            warn!(target: "arkiv-reth", %error, "failed to advance chain pruning map");
        }
    });
}

async fn catch_up<P>(
    provider: P,
    store: HostStore,
    pruning_map: ChainPruningMap,
) -> eyre::Result<()>
where
    P: StateProviderFactory
        + BlockReader<Block = reth_ethereum::Block>
        + ReceiptProvider<Receipt = reth_ethereum::Receipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let started = Instant::now();
    // No genesis walk: genesis holds no entities now that there is no seeding,
    // so the map learns everything it needs from the logs replayed below.
    let _guard = pruning_map.update_guard().await;
    let lock_wait = started.elapsed();
    let mut blocks = 0u64;
    let mut first_height = None;
    let mut last_height = None;
    loop {
        let watermark = pruning_map.watermark().await?;
        let tip = provider
            .best_block_number()
            .map_err(|error| eyre::eyre!("read chain tip: {error:?}"))?;
        if watermark >= tip {
            debug!(
                target: "arkiv::timing", blocks, first_height, last_height,
                lock_wait_ms = lock_wait.as_secs_f64() * 1000.0,
                total_ms = started.elapsed().as_secs_f64() * 1000.0,
                outcome = "caught_up",
                "Arkiv pruning catch-up finished",
            );
            return Ok(());
        }
        let height = watermark + 1;
        let block_started = Instant::now();
        let block_provider = provider.clone();
        let block_store = store.clone();
        let (entries, removed) = tokio::task::spawn_blocking(move || {
            pruning_updates_at(&block_provider, &block_store, height)
        })
        .await
        .map_err(|error| eyre::eyre!("join pruning replay for block {height}: {error}"))??;
        let replay = block_started.elapsed();
        let phase = Instant::now();
        let applied = pruning_map.apply_next(height, &entries, &removed).await;
        let apply = phase.elapsed();
        debug!(
            target: "arkiv::timing", height, entries = entries.len(), removed = removed.len(),
            replay_ms = replay.as_secs_f64() * 1000.0,
            apply_ms = apply.as_secs_f64() * 1000.0,
            total_ms = block_started.elapsed().as_secs_f64() * 1000.0,
            outcome = if applied.is_ok() { "applied" } else { "error" },
            error = ?applied.as_ref().err(),
            "Arkiv pruning block finished",
        );
        applied?;
        first_height.get_or_insert(height);
        blocks += 1;
        last_height = Some(height);
    }
}

fn pruning_updates_at<P>(
    provider: &P,
    store: &HostStore,
    height: u64,
) -> eyre::Result<(Vec<PruningEntry>, Vec<B256>)>
where
    P: StateProviderFactory
        + BlockReader<Block = reth_ethereum::Block>
        + ReceiptProvider<Receipt = reth_ethereum::Receipt>,
{
    let block = provider
        .block_by_number(height)
        .map_err(|error| eyre::eyre!("read block {height}: {error:?}"))?
        .ok_or_else(|| eyre::eyre!("block {height} is unavailable"))?;
    let receipts = provider
        .receipts_by_block(height.into())
        .map_err(|error| eyre::eyre!("read receipts for block {height}: {error:?}"))?
        .ok_or_else(|| eyre::eyre!("receipts for block {height} are unavailable"))?;
    let mut touched = BTreeSet::new();
    for log in receipts.iter().flat_map(|receipt| &receipt.logs) {
        collect_log_key(log, &mut touched);
    }
    for transaction in block.body.transactions() {
        if transaction.to() != Some(ARKIV_ADDRESS)
            || !transaction.input().starts_with(&purgeExpiredCall::SELECTOR)
        {
            continue;
        }
        if let Ok(call) = purgeExpiredCall::abi_decode(transaction.input()) {
            touched.extend(call.entityKeys);
        }
    }

    let at = store.head();
    let mut entries = Vec::with_capacity(touched.len());
    let mut removed = Vec::new();
    for key in touched {
        match store_reads::entity(&**store, at, key.0)
            .map_err(|error| eyre::eyre!("read entity {key} at block {height}: {error}"))?
        {
            Some(entity) => entries.push(PruningEntry {
                key,
                expires_at: entity.expires_at,
                // Must match the executor's purge pricing, which counts these.
                attribute_count: entity.indexed_attribute_count(),
            }),
            None => removed.push(key),
        }
    }
    Ok((entries, removed))
}

fn collect_log_key(log: &alloy_primitives::Log, keys: &mut BTreeSet<B256>) {
    if log.address != ARKIV_ADDRESS {
        return;
    }
    let topics = log.topics();
    if topics.len() > 1
        && matches!(
            topics[0],
            IEntityRegistry::EntityCreated::SIGNATURE_HASH
                | IEntityRegistry::EntityPatched::SIGNATURE_HASH
                | IEntityRegistry::ExpiryExtended::SIGNATURE_HASH
                | IEntityRegistry::OwnershipTransferred::SIGNATURE_HASH
                | IEntityRegistry::EntityDeleted::SIGNATURE_HASH
        )
    {
        keys.insert(topics[1]);
    }
}

#[derive(Debug, Clone)]
pub struct ArkivPayloadBuilder<Pool, Client, Evm> {
    client: Client,
    pool: Pool,
    evm: Evm,
    config: EthereumBuilderConfig,
    chain_id: u64,
    pruning_map: ChainPruningMap,
    store: HostStore,
    seals: Arc<BlockSeals>,
}

impl<Pool, Client, Evm> PayloadBuilder for ArkivPayloadBuilder<Pool, Client, Evm>
where
    Client: reth_storage_api::StateProviderFactory
        + reth_storage_api::BlockNumReader
        + ChainSpecProvider<ChainSpec: EthereumHardforks>
        + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
    Evm: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
{
    type Attributes = alloy_rpc_types_engine::PayloadAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<
        BuildOutcome<Self::BuiltPayload>,
        reth_payload_builder_primitives::PayloadBuilderError,
    > {
        self.build_block(args, Contents::PurgeAndPool)
    }

    /// The fallback reth races against an unfinished build when a payload is
    /// requested early. It carries the purge alone, so it is quicker to build and
    /// pruning keeps its pace.
    fn build_empty_payload(
        &self,
        config: reth_basic_payload_builder::PayloadConfig<Self::Attributes>,
    ) -> Result<Self::BuiltPayload, reth_payload_builder_primitives::PayloadBuilderError> {
        let args = BuildArguments::new(
            Default::default(),
            Default::default(),
            None,
            config,
            Default::default(),
            None,
        );
        self.build_block(args, Contents::PurgeOnly)?
            .into_payload()
            .ok_or(reth_payload_builder_primitives::PayloadBuilderError::MissingPayload)
    }
}

impl<Pool, Client, Evm> ArkivPayloadBuilder<Pool, Client, Evm>
where
    Client: reth_storage_api::StateProviderFactory
        + reth_storage_api::BlockNumReader
        + ChainSpecProvider<ChainSpec: EthereumHardforks>
        + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
    Evm: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
{
    fn build_block(
        &self,
        args: BuildArguments<alloy_rpc_types_engine::PayloadAttributes, EthBuiltPayload>,
        contents: Contents,
    ) -> Result<BuildOutcome<EthBuiltPayload>, reth_payload_builder_primitives::PayloadBuilderError>
    {
        let started = Instant::now();
        let attempt = NEXT_BUILD_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let block = args.config.parent_header.number + 1;
        let parent = args.config.parent_header.hash();
        let kind = match contents {
            Contents::PurgeAndPool => "pool",
            Contents::PurgeOnly => "purge_only",
        };
        let _span = tracing::info_span!(
            target: "arkiv::timing", "payload_build",
            attempt, payload_id = %args.config.payload_id(), height = block, %parent, kind,
        )
        .entered();
        debug!(target: "arkiv::timing", "Arkiv payload build started");
        // Build on the parent's state. Adoption commits it a moment after reth
        // makes the parent canonical. `chain_info` is reth's head here: it
        // answers from the store only when the store is ahead, and then the
        // parent is committed already.
        let is_head = || {
            self.client
                .chain_info()
                .is_ok_and(|info| info.best_hash == parent)
        };
        let phase = Instant::now();
        let committed = self.seals.wait_for_commit(&self.store, parent.0, is_head);
        let parent_wait = phase.elapsed();
        committed.map_err(|error| {
            info!(
                target: "arkiv::timing", outcome = "error", ?error,
                total_ms = started.elapsed().as_secs_f64() * 1000.0,
                parent_wait_ms = parent_wait.as_secs_f64() * 1000.0,
                "Arkiv payload build finished",
            );
            reth_payload_builder_primitives::PayloadBuilderError::other(error)
        })?;
        let purge_started = Instant::now();
        let phase = Instant::now();
        let mut pruning_skipped = false;
        let keys = self
            .pruning_map
            .select_expired(
                args.config.parent_header.number,
                block,
                MAX_PURGE_KEYS,
                PURGE_GAS_LIMIT,
            )
            .unwrap_or_else(|error| {
                pruning_skipped = true;
                debug!(target: "arkiv-reth", %error, block, "skip pruning for payload");
                Vec::new()
            });
        let pruning_select = phase.elapsed();
        let purge_keys = keys.len();
        let phase = Instant::now();
        let purge = protocol_transaction(self.chain_id, block, keys);
        let protocol_tx = phase.elapsed();
        let purge_prepare = purge_started.elapsed();
        let phase = Instant::now();
        let result = default_ethereum_payload(
            self.evm.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.config.clone(),
            args,
            |attrs| match contents {
                Contents::PurgeAndPool => Box::new(PrependBest::new(
                    purge,
                    self.pool.best_transactions_with_attributes(attrs),
                )),
                Contents::PurgeOnly => Box::new(PrependBest::new(purge, std::iter::empty())),
            },
        );
        let execution_assembly = phase.elapsed();
        let outcome = match &result {
            Ok(BuildOutcome::Better { .. }) => "better",
            Ok(BuildOutcome::Freeze(_)) => "frozen",
            Ok(BuildOutcome::Aborted { .. }) => "aborted",
            Ok(BuildOutcome::Cancelled) => "cancelled",
            Err(_) => "error",
        };
        let payload = result.as_ref().ok().and_then(|outcome| outcome.payload());
        info!(
            target: "arkiv::timing",
            outcome,
            block_hash = payload.map(|payload| tracing::field::display(payload.block().hash())),
            transactions = payload.map(|payload| payload.block().body().transactions.len()),
            purge_keys, pruning_skipped,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            parent_wait_ms = parent_wait.as_secs_f64() * 1000.0,
            purge_prepare_ms = purge_prepare.as_secs_f64() * 1000.0,
            pruning_select_ms = pruning_select.as_secs_f64() * 1000.0,
            protocol_tx_ms = protocol_tx.as_secs_f64() * 1000.0,
            execution_assembly_ms = execution_assembly.as_secs_f64() * 1000.0,
            error = ?result.as_ref().err(),
            "Arkiv payload build finished",
        );
        result
    }
}

/// What a built block carries. Every block opens with the purge.
enum Contents {
    PurgeAndPool,
    PurgeOnly,
}

fn protocol_transaction(
    chain_id: u64,
    block: u64,
    keys: Vec<alloy_primitives::B256>,
) -> Arc<ValidPoolTransaction<EthPooledTransaction>> {
    let input: Bytes = purgeExpiredCall { entityKeys: keys }.abi_encode().into();
    let mut tx = TxEip1559 {
        chain_id,
        nonce: block,
        gas_limit: PURGE_GAS_LIMIT,
        max_fee_per_gas: u128::MAX,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(ARKIV_ADDRESS),
        value: U256::ZERO,
        input,
        ..Default::default()
    };
    let signer: PrivateKeySigner = PURGE_ENVELOPE_KEY.parse().expect("valid protocol key");
    debug_assert_eq!(signer.address(), PURGE_CALLER);
    let signature = signer
        .sign_transaction_sync(&mut tx)
        .expect("sign protocol transaction");
    let signed: TransactionSigned = TxEnvelope::Eip1559(tx.into_signed(signature)).into();
    let encoded_length = signed.length();
    let recovered = signed
        .try_into_recovered()
        .expect("recover protocol transaction");
    let pooled = EthPooledTransaction::new(recovered, encoded_length);
    Arc::new(ValidPoolTransaction {
        transaction_id: reth_ethereum::pool::identifier::TransactionId::new(0u64.into(), block),
        transaction: pooled,
        propagate: false,
        timestamp: Instant::now(),
        origin: TransactionOrigin::Local,
        authority_ids: None,
    })
}

struct PrependBest<I> {
    first: Option<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    rest: I,
}
impl<I> PrependBest<I> {
    fn new(first: Arc<ValidPoolTransaction<EthPooledTransaction>>, rest: I) -> Self {
        Self {
            first: Some(first),
            rest,
        }
    }
}
impl<I: Iterator<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>> Iterator
    for PrependBest<I>
{
    type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.first.take().or_else(|| self.rest.next())
    }
}
impl<I> BestTransactions for PrependBest<I>
where
    I: BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>,
{
    fn mark_invalid(&mut self, tx: &Self::Item, kind: InvalidPoolTransactionError) {
        self.rest.mark_invalid(tx, kind)
    }
    fn no_updates(&mut self) {
        self.rest.no_updates()
    }
    fn set_skip_blobs(&mut self, skip: bool) {
        self.rest.set_skip_blobs(skip)
    }
}
