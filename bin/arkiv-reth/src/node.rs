//! [`ArkivNode`]: reth's Ethereum node types with Arkiv's two substitutions.
//!
//! This exists because reth's `EthereumNode` is hard-wired to reth's `ChainSpec`
//! type, and the minimum-base-fee rule lives in [`ArkivChainSpec`]. Everything
//! else — primitives, storage, engine types, pool, network, consensus, RPC add-ons
//! — is reth's stock Ethereum set, reused through `EthereumNode::components()`.

use alloy_rpc_types_engine::ExecutionData;
use arkiv_reth_chainspec::ArkivChainSpec;
use arkiv_reth_executor::ArkivExecutorBuilder;
use arkiv_reth_golemdbprovider::ArkivStateRootStrategy;
use arkiv_reth_payload_builder::ArkivPayloadServiceBuilder;
use arkiv_reth_statemanager::{BlockSeals, HostStore};
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadAttributesBuilder, PayloadTypes},
    builder::{DebugNode, Node, NodeAdapter, components::ComponentsBuilder},
};
use reth_engine_tree::tree::state_root_strategy::StateRootStrategy;
use reth_engine_tree::tree::{BasicEngineValidator, TreeConfig};
use reth_ethereum::{Block, EthPrimitives, engine::local::LocalPayloadAttributesBuilder};
use reth_node_api::AddOnsContext;
use reth_node_builder::ConfigureEngineEvm;
use reth_node_builder::rpc::{
    BasicEngineApiBuilder, BasicEngineValidatorBuilder, EngineValidatorBuilder,
    PayloadValidatorBuilder, RpcAddOns,
};
use reth_node_ethereum::{
    EthEngineTypes, EthereumAddOns, EthereumConsensusBuilder, EthereumEngineValidatorBuilder,
    EthereumEthApiBuilder, EthereumNetworkBuilder, EthereumNode, EthereumPoolBuilder,
};
use reth_storage_api::EthStorage;
use reth_storage_overlay::OverlayManager;
use std::sync::Arc;

/// The Arkiv node: Ethereum node types on [`ArkivChainSpec`], with the no-EVM
/// executor in place of reth's EVM.
#[derive(Debug, Clone)]
pub struct ArkivNode {
    store: HostStore,
    seals: Arc<BlockSeals>,
}

impl ArkivNode {
    /// The node over the store Arkiv's state lives in.
    pub const fn new(store: HostStore, seals: Arc<BlockSeals>) -> Self {
        Self { store, seals }
    }
}

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
    type AddOns = EthereumAddOns<
        NodeAdapter<N>,
        EthereumEthApiBuilder,
        EthereumEngineValidatorBuilder,
        BasicEngineApiBuilder<EthereumEngineValidatorBuilder>,
        ArkivEngineValidatorBuilder,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        EthereumNode::components()
            .executor(ArkivExecutorBuilder::new(
                self.store.clone(),
                self.seals.clone(),
            ))
            .payload(ArkivPayloadServiceBuilder::new(
                self.store.clone(),
                self.seals.clone(),
            ))
    }

    /// Spelt out rather than `EthereumAddOns::default()`: the default pins the
    /// engine validator builder, and Arkiv's is what installs the state-root
    /// strategy that puts GolemDB's root in the header.
    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::new(RpcAddOns::new(
            EthereumEthApiBuilder::default(),
            EthereumEngineValidatorBuilder::default(),
            BasicEngineApiBuilder::default(),
            ArkivEngineValidatorBuilder::new(self.seals.clone()),
            Default::default(),
            reth_node_builder::rpc::Identity::new(),
        ))
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

/// Builds the engine validator with Arkiv's state-root strategy installed.
///
/// `with_state_root_strategy` is a method on the built validator rather than
/// something the add-ons expose, so the only way to reach it is to wrap the
/// stock builder and call it on the way out. reth's
/// `examples/custom-state-root` does the same.
///
/// It lives here, beside [`ArkivNode`], because the bounds only resolve against
/// a concrete node type: `BasicEngineValidator`'s payload validator has to be
/// pinned to Ethereum's block and execution-data types, which `Types = Self`
/// supplies and a structural bound does not.
#[derive(Clone)]
pub struct ArkivEngineValidatorBuilder {
    inner: BasicEngineValidatorBuilder<EthereumEngineValidatorBuilder>,
    seals: Arc<BlockSeals>,
}

impl ArkivEngineValidatorBuilder {
    pub fn new(seals: Arc<BlockSeals>) -> Self {
        Self {
            inner: BasicEngineValidatorBuilder::default(),
            seals,
        }
    }
}

impl std::fmt::Debug for ArkivEngineValidatorBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArkivEngineValidatorBuilder")
            .finish_non_exhaustive()
    }
}

impl<N> EngineValidatorBuilder<N> for ArkivEngineValidatorBuilder
where
    N: FullNodeComponents<Types = ArkivNode, Evm: ConfigureEngineEvm<ExecutionData>>,
{
    type EngineValidator = BasicEngineValidator<
        N::Provider,
        N::Evm,
        <EthereumEngineValidatorBuilder as PayloadValidatorBuilder<N>>::Validator,
    >;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, N>,
        tree_config: TreeConfig,
        overlay_manager: OverlayManager<EthPrimitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self
            .inner
            .build_tree_validator(ctx, tree_config, overlay_manager)
            .await?;
        let strategy: Arc<dyn StateRootStrategy<EthPrimitives, N::Provider, N::Evm>> =
            Arc::new(ArkivStateRootStrategy::new(self.seals));
        Ok(validator.with_state_root_strategy(strategy))
    }
}
