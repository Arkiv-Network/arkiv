//! [`ArkivNode`]: reth's Ethereum node types with Arkiv's two substitutions.
//!
//! This exists because reth's `EthereumNode` is hard-wired to reth's `ChainSpec`
//! type, and the minimum-base-fee rule lives in [`ArkivChainSpec`]. Everything
//! else — primitives, storage, engine types, pool, network, consensus, RPC add-ons
//! — is reth's stock Ethereum set, reused through `EthereumNode::components()`.

use arkiv_reth_chainspec::ArkivChainSpec;
use arkiv_reth_executor::ArkivExecutorBuilder;
use arkiv_reth_payload_builder::ArkivPayloadServiceBuilder;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadAttributesBuilder, PayloadTypes},
    builder::{
        BuilderContext, DebugNode, Node, NodeAdapter,
        components::{ComponentsBuilder, NodeComponentsBuilder},
    },
};
use reth_ethereum::{Block, EthPrimitives, engine::local::LocalPayloadAttributesBuilder};
use reth_node_ethereum::{
    EthEngineTypes, EthereumAddOns, EthereumConsensusBuilder, EthereumEngineValidatorBuilder,
    EthereumEthApiBuilder, EthereumNetworkBuilder, EthereumNode, EthereumPoolBuilder,
};
use reth_storage_api::EthStorage;
use std::sync::Arc;

/// The Arkiv node: Ethereum node types on [`ArkivChainSpec`], with the no-EVM
/// executor in place of reth's EVM.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct ArkivNode;

impl NodeTypes for ArkivNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ArkivChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

/// reth's Ethereum component set with the executor swapped for Arkiv's.
type InnerComponentsBuilder<N> = ComponentsBuilder<
    N,
    EthereumPoolBuilder,
    ArkivPayloadServiceBuilder,
    EthereumNetworkBuilder,
    ArkivExecutorBuilder,
    EthereumConsensusBuilder,
>;

/// Check imported genesis state before starting the network, payload service or
/// pruning bootstrap. An initialized genesis header alone is not proof of state.
pub struct ArkivComponentsBuilder<N>(InnerComponentsBuilder<N>);

impl<N: FullNodeTypes<Types = ArkivNode>> NodeComponentsBuilder<N> for ArkivComponentsBuilder<N> {
    type Components = <InnerComponentsBuilder<N> as NodeComponentsBuilder<N>>::Components;

    async fn build_components(self, ctx: &BuilderContext<N>) -> eyre::Result<Self::Components> {
        crate::init_state::status::ensure_complete(ctx.provider(), &ctx.chain_spec())?;
        use reth_ethereum::chainspec::EthChainSpec;
        use reth_storage_api::StateProviderFactory;
        let records = arkiv_authenticated_store::Store::open(
            ctx.config().datadir().data_dir().join("arkiv-state"),
        )?;
        arkiv_reth_statemanager::genesis::initialize(ctx.chain_spec().genesis(), records.clone())?;
        // Fail before serving requests if the execution database was copied without
        // its authenticated records. Never silently start a second, empty state.
        arkiv_reth_rpc::snapshot::authenticated_snapshot(ctx.provider().latest()?, records)?;
        self.0.build_components(ctx).await
    }
}

impl<N> Node<N> for ArkivNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ArkivComponentsBuilder<N>;
    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ArkivComponentsBuilder(
            EthereumNode::components()
                .executor(ArkivExecutorBuilder::default())
                .payload(ArkivPayloadServiceBuilder),
        )
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}

/// What `launch_with_debug_capabilities` needs: the `--dev` local miner's payload
/// attributes and the RPC block shape for `--debug.*` replay. Same as reth's.
impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for ArkivNode {
    type RpcBlock = alloy_rpc_types::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> Block {
        rpc_block.into_consensus().convert_transactions()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes> {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}
