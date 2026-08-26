//! Arkiv payload builder: prepend a bounded, protocol-generated purge transaction.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, transaction::SignerRecoverable};
use alloy_network::TxSignerSync;
use alloy_primitives::{Bytes, TxKind, U256};
use alloy_rlp::Encodable;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use arkiv_bindings::{IEntityRegistry, MAX_ATTRIBUTES, MAX_PURGE_KEYS, PURGE_GAS_THRESHOLD};
use arkiv_reth_executor::{ARKIV_ADDRESS, expiry_queue::expiry_queue};
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadBuilder};
use reth_ethereum::{
    EthPrimitives, TransactionSigned,
    chainspec::ChainSpec,
    evm::primitives::{ConfigureEvm, NextBlockEnvAttributes},
    node::{
        api::{FullNodeTypes, NodeTypes},
        builder::{BuilderContext, PayloadBuilderConfig, components::PayloadBuilderBuilder},
    },
    pool::{
        BestTransactions, EthPooledTransaction, TransactionOrigin, TransactionPool,
        ValidPoolTransaction, error::InvalidPoolTransactionError,
    },
};
use reth_ethereum_payload_builder::{EthereumBuilderConfig, default_ethereum_payload};
use reth_node_ethereum::EthEngineTypes;
use reth_payload_builder::EthBuiltPayload;
use std::{sync::Arc, time::Instant};

/// Public protocol material, not an authentication secret.
const PURGE_ENVELOPE_KEY: &str = "8b3a350cf5c34c9194ca3a545d4b54b69356a5f5a39d9c7f94a17e5f7f9a6c31";
const PURGE_GAS_LIMIT: u64 =
    PURGE_GAS_THRESHOLD + arkiv_interfaces::gas::purge_cost(MAX_ATTRIBUTES);

#[derive(Debug, Default, Clone, Copy)]
pub struct ArkivPayloadBuilderBuilder;

impl<Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for ArkivPayloadBuilderBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
            Payload = EthEngineTypes,
        >,
    >,
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Unpin + 'static,
    Evm: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>
        + 'static,
{
    type PayloadBuilder = ArkivPayloadBuilder<Pool, Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(ArkivPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm,
            chain_id: ctx.chain_spec().chain().id(),
            config: EthereumBuilderConfig::new()
                .with_extra_data(ctx.payload_builder_config().extra_data()),
        })
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
        + reth_ethereum::chainspec::ChainSpecProvider<ChainSpec = ChainSpec>
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
            // `expires_at` is the entity's first non-live block. Purging starts
            // in the following block so expiry and physical cleanup remain
            // distinct lifecycle steps.
            .select_expired(block.saturating_sub(1), MAX_PURGE_KEYS, PURGE_GAS_THRESHOLD);
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
    let input: Bytes = IEntityRegistry::purgeExpiredCall { entityKeys: keys }
        .abi_encode()
        .into();
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
