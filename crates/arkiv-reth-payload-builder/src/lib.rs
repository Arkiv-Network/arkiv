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
use arkiv_reth_statemanager::{BlockSeals, HostStore};
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
                    // The block is canonical, so its Arkiv state is no longer
                    // speculative: promote the seal to a commit. This is the
                    // only place anything is written.
                    let tip = notification.tip();
                    let height = tip.number;
                    match seals.adopt(&store, height, tip.hash().0) {
                        Ok(Some(commit)) => {
                            debug!(target: "arkiv-reth", height, commit = commit.0, "adopted the block's Arkiv state")
                        }
                        Ok(None) => {}
                        Err(error) => {
                            warn!(target: "arkiv-reth", ?error, height, "failed to adopt the block's Arkiv state")
                        }
                    }
                    if let Err(error) = catch_up(provider, store, pruning_map).await {
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
    // No genesis walk: genesis holds no entities now that there is no seeding,
    // so the map learns everything it needs from the logs replayed below.
    let _guard = pruning_map.update_guard().await;
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
        let block_store = store.clone();
        let (entries, removed) = tokio::task::spawn_blocking(move || {
            pruning_updates_at(&block_provider, &block_store, height)
        })
        .await
        .map_err(|error| eyre::eyre!("join pruning replay for block {height}: {error}"))??;
        pruning_map.apply_next(height, &entries, &removed).await?;
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
