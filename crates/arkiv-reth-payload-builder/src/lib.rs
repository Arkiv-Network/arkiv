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
use arkiv_reth_executor::ARKIV_ADDRESS;
use arkiv_reth_rpc::snapshot::SnapshotView;
use arkiv_store::ArkivDb;
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
use reth_ethereum_payload_builder::{EthereumBuilderConfig, default_ethereum_payload};
use reth_node_ethereum::EthEngineTypes;
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderHandle, PayloadBuilderService};
use reth_storage_api::{BlockReader, ReceiptProvider, StateProviderFactory};
use std::{collections::BTreeSet, sync::Arc, time::Instant};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

/// Public protocol material, not an authentication secret.
const PURGE_ENVELOPE_KEY: &str = "8b3a350cf5c34c9194ca3a545d4b54b69356a5f5a39d9c7f94a17e5f7f9a6c31";

/// Genesis entities folded into the pruning map per transaction during the
/// one-time bootstrap walk.
const GENESIS_BOOTSTRAP_BATCH: usize = 10_000;
/// Chunks resolved at once, each on its own thread with its own snapshots.
/// The reads are random pages of a state far larger than memory, so a worker
/// mostly waits on one page fault at a time; the count is what keeps the disk
/// busy, not the cores (capped by the cores available all the same).
const GENESIS_BOOTSTRAP_WORKERS: usize = 32;
/// Entries committed to the map per transaction. Genesis keys are hashes, so
/// a commit rewrites about one leaf page per row up to the whole table
/// however small the batch; large batches keep that amplification down.
const GENESIS_BOOTSTRAP_ROWS_PER_COMMIT: usize = 1_000_000;

#[derive(Debug, Default, Clone, Copy)]
pub struct ArkivPayloadServiceBuilder;

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
        let db = ArkivDb::shared(
            &arkiv_reth_executor::arkiv_db_path(ctx.config().datadir().data_dir()),
            Handle::current(),
        )?;
        let payload_builder = ArkivPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm,
            chain_id: ctx.chain_spec().chain().id(),
            pruning_map: pruning_map.clone(),
            config: EthereumBuilderConfig::new()
                .with_extra_data(ctx.payload_builder_config().extra_data()),
        };
        let conf = ctx.config().builder.clone();
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
            db.clone(),
            pruning_map.clone(),
        );
        let provider = ctx.provider().clone();
        let notifications = Box::pin(ctx.provider().canonical_state_stream().then(
            move |notification| {
                let provider = provider.clone();
                let db = db.clone();
                let pruning_map = pruning_map.clone();
                async move {
                    if let Err(error) = catch_up(provider, db, pruning_map).await {
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

fn spawn_catch_up<P>(
    executor: &reth_ethereum::tasks::TaskExecutor,
    provider: P,
    db: Arc<ArkivDb>,
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
        if let Err(error) = catch_up(provider, db, pruning_map).await {
            warn!(target: "arkiv-reth", %error, "failed to advance chain pruning map");
        }
    });
}

async fn catch_up<P>(
    provider: P,
    db: Arc<ArkivDb>,
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
    let _guard = pruning_map.update_guard().await;
    if !pruning_map.genesis_bootstrapped().await? {
        bootstrap_genesis(provider.clone(), db.clone(), pruning_map.clone()).await?;
    }
    loop {
        let watermark = pruning_map.watermark().await?;
        let tip = provider
            .best_block_number()
            .map_err(|error| eyre::eyre!("read chain tip: {error:?}"))?;
        if watermark >= tip {
            return Ok(());
        }
        let height = watermark + 1;
        let block_provider = provider.clone();
        let db = db.clone();
        let (entries, removed) =
            tokio::task::spawn_blocking(move || pruning_updates_at(&block_provider, db, height))
                .await
                .map_err(|error| {
                    eyre::eyre!("join pruning replay for block {height}: {error}")
                })??;
        pruning_map.apply_next(height, &entries, &removed).await?;
    }
}

/// Fold the entities present at block 0 into the map.
///
/// A seeded genesis holds entities no block ever created, so the replay in
/// [`catch_up`] — which learns about entities from their operation logs —
/// would never schedule them for purging. Walk the genesis `$all` bucket once
/// and record every entity that still exists, as it stands; the replay of the
/// blocks after the watermark then keeps them current like any other. The map
/// remembers that the walk ran, so a restart does not repeat it, and how far
/// it got, so an interrupted walk resumes.
async fn bootstrap_genesis<P>(
    provider: P,
    db: Arc<ArkivDb>,
    pruning_map: ChainPruningMap,
) -> eyre::Result<()>
where
    P: StateProviderFactory + Clone + Send + Sync + 'static,
{
    let watermark = pruning_map.watermark().await?;
    let map = pruning_map.clone();
    let entities =
        tokio::task::spawn_blocking(move || bootstrap_genesis_blocking(&provider, db, &map))
            .await
            .map_err(|error| eyre::eyre!("join genesis pruning bootstrap: {error}"))??;
    pruning_map.mark_genesis_bootstrapped().await?;
    info!(target: "arkiv-reth", entities, watermark, "bootstrapped the chain pruning map from genesis");
    Ok(())
}

/// The synchronous half of [`bootstrap_genesis`]: read the genesis `$all`
/// bucket, then resolve the entities as they stand now in chunks and upsert
/// them batch by batch, moving the map's cursor with each batch (see
/// [`resolve_genesis_entities`] for why now rather than at the watermark).
///
/// Every chunk opens its own state snapshots. reth aborts a read transaction
/// held open past its limit (five minutes by default), and one snapshot over
/// a walk of tens of millions of entities would take hours, failing part-way
/// and starting over on the next attempt. The cursor makes an interrupted
/// walk (a restart, or such a failure) resume where it stopped; upserts make
/// a repeated batch harmless. Chunks are resolved on a few threads at once,
/// since each is independent provider reads.
fn bootstrap_genesis_blocking<P>(
    provider: &P,
    db: Arc<ArkivDb>,
    pruning_map: &ChainPruningMap,
) -> eyre::Result<u64>
where
    P: StateProviderFactory + Sync,
{
    // The genesis entity keys, ascending. The map's cursor counts how many of
    // them an earlier attempt already wrote.
    let keys: Vec<B256> = {
        let genesis = provider
            .history_by_block_number(0)
            .map_err(|error| eyre::eyre!("read genesis state: {error:?}"))?;
        let snapshot = SnapshotView::open(&genesis, db.clone())?;
        let view = snapshot.view();
        view.entity_keys()
            .map_err(|error| eyre::eyre!("walk the genesis entities: {error:?}"))?
            .map(|key| key.map(B256::from))
            .collect::<Result<_, _>>()
            .map_err(|error| eyre::eyre!("walk the genesis entities: {error:?}"))?
    };
    if keys.is_empty() {
        return Ok(0);
    }
    let total = keys.len() as u64;
    let next = pruning_map.genesis_cursor_blocking()? as usize;
    if next > 0 {
        info!(target: "arkiv-reth", next, total, "resuming the genesis pruning bootstrap");
    }
    let mut writer = pruning_map.genesis_bootstrap_writer_blocking()?;
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, GENESIS_BOOTSTRAP_WORKERS);

    let mut inserted = 0u64;
    let mut pending: Vec<PruningEntry> = Vec::with_capacity(GENESIS_BOOTSTRAP_ROWS_PER_COMMIT);
    let mut resolving = std::time::Duration::ZERO;
    let mut position = next.min(keys.len());
    while position < keys.len() {
        let window_end = (position + workers * GENESIS_BOOTSTRAP_BATCH).min(keys.len());
        let chunks: Vec<&[B256]> = keys[position..window_end]
            .chunks(GENESIS_BOOTSTRAP_BATCH)
            .collect();
        let started = Instant::now();
        let resolved = std::thread::scope(|scope| {
            let handles: Vec<_> = chunks
                .iter()
                .map(|chunk| {
                    let db = db.clone();
                    scope.spawn(move || resolve_genesis_entities(provider, db, chunk))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| eyre::eyre!("a genesis pruning bootstrap worker panicked"))?
                })
                .collect::<eyre::Result<Vec<Vec<PruningEntry>>>>()
        })?;
        pending.extend(resolved.into_iter().flatten());
        resolving += started.elapsed();
        position = window_end;
        if pending.len() < GENESIS_BOOTSTRAP_ROWS_PER_COMMIT && position < keys.len() {
            continue;
        }
        let started = Instant::now();
        writer.write_blocking(&mut pending, position as u64)?;
        inserted += pending.len() as u64;
        info!(
            target: "arkiv-reth",
            inserted,
            next = position,
            total,
            resolve_ms = resolving.as_millis(),
            write_ms = started.elapsed().as_millis(),
            "genesis pruning bootstrap in progress"
        );
        pending.clear();
        resolving = std::time::Duration::ZERO;
    }
    Ok(inserted)
}

/// The pruning entries of the genesis entities `keys` as they stand now, over
/// a snapshot of the latest state opened for just this call. A key whose
/// entity is gone by then is skipped.
///
/// The latest state, not the state at the watermark: reading past it is
/// harmless. The blocks between it and the tip are replayed after the walk,
/// and that replay records each entity's expiry as an absolute value and
/// removals as removals, so whatever this walk saw is overwritten by the same
/// facts.
fn resolve_genesis_entities<P>(
    provider: &P,
    db: Arc<ArkivDb>,
    keys: &[B256],
) -> eyre::Result<Vec<PruningEntry>>
where
    P: StateProviderFactory,
{
    let latest = provider
        .latest()
        .map_err(|error| eyre::eyre!("read the latest state: {error:?}"))?;
    let snapshot = SnapshotView::open(&latest, db)?;
    let view = snapshot.view();

    let mut entries = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(entity) = view
            .entity(&key.0)
            .map_err(|error| eyre::eyre!("read genesis entity {key}: {error:?}"))?
        {
            entries.push(PruningEntry {
                key: *key,
                expires_at: entity.expires_at,
                attribute_count: entity.attributes.len(),
            });
        }
    }
    Ok(entries)
}

fn pruning_updates_at<P>(
    provider: &P,
    db: Arc<ArkivDb>,
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

    let state = provider
        .history_by_block_number(height)
        .map_err(|error| eyre::eyre!("read state for block {height}: {error:?}"))?;
    let snapshot = SnapshotView::open(&state, db)?;
    let view = snapshot.view();
    let mut entries = Vec::with_capacity(touched.len());
    let mut removed = Vec::new();
    for key in touched {
        match view
            .entity(&key.0)
            .map_err(|error| eyre::eyre!("read entity {key} at block {height}: {error:?}"))?
        {
            Some(entity) => entries.push(PruningEntry {
                key,
                expires_at: entity.expires_at,
                attribute_count: entity.attributes.len(),
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
}

impl<Pool, Client, Evm> PayloadBuilder for ArkivPayloadBuilder<Pool, Client, Evm>
where
    Client: reth_storage_api::StateProviderFactory
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
        let block = args.config.parent_header.number + 1;
        let keys = self
            .pruning_map
            .select_expired(
                args.config.parent_header.number,
                block,
                MAX_PURGE_KEYS,
                PURGE_GAS_LIMIT,
            )
            .unwrap_or_else(|error| {
                debug!(target: "arkiv-reth", %error, block, "skip pruning for payload");
                Vec::new()
            });
        let purge = protocol_transaction(self.chain_id, block, keys);
        default_ethereum_payload(
            self.evm.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.config.clone(),
            args,
            |attrs| {
                Box::new(PrependBest::new(
                    purge,
                    self.pool.best_transactions_with_attributes(attrs),
                ))
            },
        )
    }

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
        self.try_build(args)?
            .into_payload()
            .ok_or(reth_payload_builder_primitives::PayloadBuilderError::MissingPayload)
    }
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
