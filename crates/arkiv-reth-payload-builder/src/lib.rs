//! Arkiv payload builder: prepend a bounded, protocol-generated purge transaction.

use alloy_consensus::{
    BlockHeader, SignableTransaction, Transaction, TxEip1559, TxEnvelope,
    transaction::SignerRecoverable,
};
use alloy_network::TxSignerSync;
use alloy_primitives::{B256, Bytes, TxKind, U256};
use alloy_rlp::Encodable;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use arkiv_bindings::{
    IEntityRegistry, MAX_PURGE_KEYS, PURGE_CALLER, PURGE_GAS_LIMIT, protocol::purgeExpiredCall,
};
use arkiv_reth_executor::{ARKIV_ADDRESS, expiry_queue::expiry_queue};
use arkiv_reth_mpt_committed_store::{CodeBackend, RethEntityStore};
use arkiv_reth_rpc::snapshot::SnapshotAccountCode;
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
    provider::{CanonStateNotification, CanonStateSubscriptions, Chain},
};
use reth_ethereum_payload_builder::{EthereumBuilderConfig, default_ethereum_payload};
use reth_node_ethereum::EthEngineTypes;
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderHandle, PayloadBuilderService};
use reth_storage_api::StateProviderFactory;
use std::{collections::BTreeSet, sync::Arc, time::Instant};
use tracing::warn;

/// Public protocol material, not an authentication secret.
const PURGE_ENVELOPE_KEY: &str = "8b3a350cf5c34c9194ca3a545d4b54b69356a5f5a39d9c7f94a17e5f7f9a6c31";

#[derive(Debug, Default, Clone, Copy)]
pub struct ArkivPayloadServiceBuilder;

impl<Node, Pool, Evm> PayloadServiceBuilder<Node, Pool, Evm> for ArkivPayloadServiceBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<Primitives = EthPrimitives, Payload = EthEngineTypes>>,
    <Node::Types as NodeTypes>::ChainSpec: EthChainSpec + EthereumHardforks,
    Node::Provider: StateProviderFactory + Unpin,
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
        let payload_builder = ArkivPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm,
            chain_id: ctx.chain_spec().chain().id(),
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
        let provider = ctx.provider().clone();
        let notifications = ctx
            .provider()
            .canonical_state_stream()
            .inspect(move |notification| {
                if let Err(error) = apply_canonical_update(&provider, notification) {
                    warn!(target: "arkiv-reth", %error, "failed to update expiry queue");
                }
            });
        let (service, handle) = PayloadBuilderService::new(generator, notifications);
        ctx.task_executor().spawn_critical_os_thread(
            "payload-service",
            "arkiv payload builder service",
            service,
        );
        Ok(handle)
    }
}

fn apply_canonical_update<P>(
    provider: &P,
    notification: &CanonStateNotification<EthPrimitives>,
) -> eyre::Result<()>
where
    P: StateProviderFactory,
{
    let committed = notification.committed();
    let reverted = notification.reverted();
    let mut touched = BTreeSet::new();
    collect_touched_keys(&committed, &mut touched);
    if let Some(reverted) = &reverted {
        collect_touched_keys(reverted, &mut touched);
    }
    if touched.is_empty() {
        return Ok(());
    }

    let state_hash = (!committed.is_empty())
        .then(|| committed.tip().hash())
        .or_else(|| {
            reverted
                .as_ref()
                .map(|chain| chain.first().header().parent_hash())
        });
    let Some(state_hash) = state_hash else {
        return Ok(());
    };
    let state = provider
        .state_by_block_hash(state_hash)
        .map_err(|error| eyre::eyre!("canonical state {state_hash}: {error:?}"))?;
    let mut entities = RethEntityStore::new(CodeBackend::new(SnapshotAccountCode::new(state)));
    let mut queue = expiry_queue()
        .write()
        .map_err(|_| eyre::eyre!("expiry queue poisoned"))?;
    for key in touched {
        match entities
            .get(key.0)
            .map_err(|error| eyre::eyre!("read canonical entity {key}: {error:?}"))?
        {
            Some(entity) => queue.insert(key, entity.expires_at, entity.attributes.len()),
            None => queue.remove(key),
        }
    }
    Ok(())
}

fn collect_touched_keys(chain: &Chain<EthPrimitives>, keys: &mut BTreeSet<B256>) {
    for log in chain.logs_iter() {
        if log.address != ARKIV_ADDRESS {
            continue;
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
    for transaction in chain.transactions_iter() {
        if transaction.to() != Some(ARKIV_ADDRESS)
            || !transaction.input().starts_with(&purgeExpiredCall::SELECTOR)
        {
            continue;
        }
        if let Ok(call) = purgeExpiredCall::abi_decode(transaction.input()) {
            keys.extend(call.entityKeys);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArkivPayloadBuilder<Pool, Client, Evm> {
    client: Client,
    pool: Pool,
    evm: Evm,
    config: EthereumBuilderConfig,
    chain_id: u64,
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
        let keys = expiry_queue()
            .read()
            .expect("expiry queue poisoned")
            // `expires_at` is the entity's first non-live block, so entities due
            // at this height are eligible. Writes in this block must resolve to
            // a later expiry and cannot enter this selection.
            .select_expired(block, MAX_PURGE_KEYS, PURGE_GAS_LIMIT);
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
