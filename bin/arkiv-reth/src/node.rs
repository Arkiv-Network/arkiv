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
    builder::{DebugNode, Node, NodeAdapter, components::ComponentsBuilder},
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

impl<N> Node<N> for ArkivNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = InnerComponentsBuilder<N>;
    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        EthereumNode::components()
            .executor(ArkivExecutorBuilder::default())
            .payload(ArkivPayloadServiceBuilder)
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
