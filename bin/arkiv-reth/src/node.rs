//! [`ArkivNode`]: reth's Ethereum node types with Arkiv's two substitutions.
//!
//! This exists because reth's `EthereumNode` is hard-wired to reth's `ChainSpec`
//! type, and the minimum-base-fee rule lives in [`ArkivChainSpec`]. Everything
//! else — primitives, storage, engine types, pool, network, consensus, RPC add-ons
//! — is reth's stock Ethereum set, reused through `EthereumNode::components()`.

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionData, PayloadAttributes as EthPayloadAttributes};
use arkiv_interfaces::gas::OpCost;
use arkiv_reth_chainspec::ArkivChainSpec;
use arkiv_reth_executor::ArkivExecutorBuilder;
use arkiv_reth_golemdbprovider::ArkivStateRootStrategy;
use arkiv_reth_payload_builder::ArkivPayloadServiceBuilder;
use arkiv_reth_statemanager::segments::rows_for_block;
use arkiv_reth_statemanager::{BlockSeals, HostStore};
use reth::network::p2p::full_block::SealedBlockWithAccessList;
use reth::payload::{BuiltPayloadExecutedBlock, PayloadBuilderResources};
use reth::providers::ProviderResult;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadAttributesBuilder, PayloadTypes},
    builder::{DebugNode, Node, NodeAdapter, components::ComponentsBuilder},
};
use reth_chain_state::ExecutedBlock;
use reth_engine_tree::tree::payload_validator::TreeCtx;
use reth_engine_tree::tree::state_root_strategy::StateRootStrategy;
use reth_engine_tree::tree::{
    BasicEngineValidator, CacheWaitDurations, EngineApiTreeState, EngineValidator, TreeConfig,
    ValidationOutcome, WaitForCaches,
};
use reth_ethereum::{
    Block, EthPrimitives,
    engine::local::LocalPayloadAttributesBuilder,
    primitives::{Header, SealedBlock},
};
use reth_node_api::{
    AddOnsContext, EngineApiMessageVersion, EngineApiValidator, EngineObjectValidationError,
    InvalidPayloadAttributesError, NewPayloadError, PayloadOrAttributes, PayloadValidator,
};
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
use tracing::{debug, warn};

/// The Arkiv node: Ethereum node types on [`ArkivChainSpec`], with the no-EVM
/// executor in place of reth's EVM.
#[derive(Debug, Clone)]
pub struct ArkivNode {
    store: HostStore,
    seals: Arc<BlockSeals>,
    op_cost: OpCost,
}

impl ArkivNode {
    /// The node over the store Arkiv's state lives in, with what an op costs
    /// beyond that store's receipts.
    pub const fn new(store: HostStore, seals: Arc<BlockSeals>, op_cost: OpCost) -> Self {
        Self {
            store,
            seals,
            op_cost,
        }
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
        ArkivPayloadValidatorBuilder,
        BasicEngineApiBuilder<ArkivPayloadValidatorBuilder>,
        ArkivEngineValidatorBuilder,
    >;

    /// Arkiv maintains no Merkle trie, so the stages that build and verify one
    /// must not run.
    ///
    /// Since the header carries GolemDB's state root rather than an MPT root,
    /// `MerkleExecute` recomputes a root from reth's own tables and compares
    /// it against a number it can never equal. The staged pipeline runs on
    /// startup, so a node that had persisted blocks would unwind, fail the
    /// comparison and exit:
    ///
    /// ```text
    /// mismatched block state root: got 0x2b98…, expected 0xda47…
    /// ```
    ///
    /// Disabling them loses nothing Arkiv had. The root in the header is
    /// GolemDB's, computed at seal, and reth's engine still checks the block
    /// it executes against it — what goes is a second, contradictory check
    /// against a trie this node does not keep.
    ///
    /// The hashing stages that feed them are left on: they only populate
    /// tables, so they are wasted work rather than a wrong answer, and
    /// removing them is a separate question from making the node start.
    fn disabled_stages() -> &'static [reth_stages_types::StageId] {
        &[
            reth_stages_types::StageId::MerkleExecute,
            reth_stages_types::StageId::MerkleUnwind,
        ]
    }

    fn components_builder(&self) -> Self::ComponentsBuilder {
        EthereumNode::components()
            .executor(ArkivExecutorBuilder::new(
                self.store.clone(),
                self.seals.clone(),
                self.op_cost,
            ))
            .payload(ArkivPayloadServiceBuilder::new(
                self.store.clone(),
                self.seals.clone(),
            ))
    }

    /// Spelt out rather than `EthereumAddOns::default()`: the default pins the
    /// engine validator builder, and Arkiv's is what installs the state-root
    /// strategy that puts GolemDB's root in the header and the payload
    /// validator that refuses withdrawals.
    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::new(RpcAddOns::new(
            EthereumEthApiBuilder::default(),
            ArkivPayloadValidatorBuilder,
            BasicEngineApiBuilder::default(),
            ArkivEngineValidatorBuilder::new(self.store.clone(), self.seals.clone()),
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
    inner: BasicEngineValidatorBuilder<ArkivPayloadValidatorBuilder>,
    store: HostStore,
    seals: Arc<BlockSeals>,
}

impl ArkivEngineValidatorBuilder {
    pub fn new(store: HostStore, seals: Arc<BlockSeals>) -> Self {
        Self {
            inner: BasicEngineValidatorBuilder::new(ArkivPayloadValidatorBuilder),
            store,
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
    type EngineValidator = ArkivEngineValidator<
        BasicEngineValidator<
            N::Provider,
            N::Evm,
            <ArkivPayloadValidatorBuilder as PayloadValidatorBuilder<N>>::Validator,
        >,
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
            Arc::new(ArkivStateRootStrategy::new(self.seals.clone()));
        Ok(ArkivEngineValidator {
            inner: validator.with_state_root_strategy(strategy),
            store: self.store,
            seals: self.seals,
        })
    }
}

/// reth's engine validator, saving a block's parent to the store before the
/// block executes.
///
/// A block executes only over a parent the store has saved, and the store
/// saves a block when reth makes it canonical. But reth also executes blocks
/// whose parent is not canonical yet: when the consensus client sends a block
/// before making its parent the head, and when reth downloads a gap and
/// executes it block by block. So the parent is saved here first.
///
/// That is safe because reth executes a block only once it has executed its
/// parent and found it valid, and the parent's state was sealed then. Saving
/// it here is the same commit adoption makes once the parent is canonical,
/// made earlier. The cost: a parent whose child is executed can no longer be
/// reorged out, which Arkiv does not support for a canonical block either.
pub struct ArkivEngineValidator<V> {
    inner: V,
    store: HostStore,
    seals: Arc<BlockSeals>,
}

impl<V> ArkivEngineValidator<V> {
    /// Save `parent` if it is an executed block the store has not saved, and
    /// its own parent is saved. Anything else is left for execution to report:
    /// it refuses a parent the store does not have.
    fn save_parent(&self, parent: B256, ctx: &TreeCtx<'_, EthPrimitives>) {
        if let Err(error) = self.try_save_parent(parent, ctx) {
            warn!(target: "arkiv-reth", %parent, %error, "did not save the block before executing its child");
        }
    }

    fn try_save_parent(&self, parent: B256, ctx: &TreeCtx<'_, EthPrimitives>) -> eyre::Result<()> {
        let saved = self
            .store
            .commit_by_tag(parent.0)
            .map_err(|error| eyre::eyre!("commit_by_tag: {error:?}"))?;
        let executed = ctx.state().tree_state().executed_block_by_hash(parent);
        let (None, Some(executed)) = (saved, executed) else {
            return Ok(());
        };
        let block = executed.recovered_block();
        // Its own parent first: saved already, or reth's head with its
        // adoption on the way.
        let grandparent = block.parent_hash;
        let canonical = ctx.canonical_in_memory_state();
        self.seals.wait_for_commit(&self.store, grandparent.0, || {
            canonical.chain_info().best_hash == grandparent
        })?;
        let writes = rows_for_block(
            block.header(),
            &block.body().transactions,
            block.senders(),
            &executed.execution_outcome().result.receipts,
            parent,
        )?;
        let saved = self
            .seals
            .adopt(
                &self.store,
                block.number,
                parent.0,
                block.state_root.0,
                &writes,
            )
            .map_err(|error| eyre::eyre!("adopt: {error:?}"))?;
        if saved.is_none() {
            eyre::bail!("no sealed state has the block's state root");
        }
        debug!(target: "arkiv-reth", number = block.number, %parent, "saved the block before executing its child");
        Ok(())
    }
}

impl<V: EngineValidator<EthEngineTypes>> EngineValidator<EthEngineTypes>
    for ArkivEngineValidator<V>
{
    fn validate_payload_attributes_against_header(
        &self,
        attr: &EthPayloadAttributes,
        header: &Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        self.inner
            .validate_payload_attributes_against_header(attr, header)
    }

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Block>, NewPayloadError> {
        self.inner.convert_payload_to_block(payload)
    }

    fn validate_payload(
        &mut self,
        payload: ExecutionData,
        ctx: TreeCtx<'_, EthPrimitives>,
    ) -> ValidationOutcome<EthPrimitives> {
        self.save_parent(payload.parent_hash(), &ctx);
        self.inner.validate_payload(payload, ctx)
    }

    fn validate_block(
        &mut self,
        block: SealedBlockWithAccessList<Block>,
        ctx: TreeCtx<'_, EthPrimitives>,
    ) -> ValidationOutcome<EthPrimitives> {
        self.save_parent(block.header().parent_hash, &ctx);
        self.inner.validate_block(block, ctx)
    }

    fn on_inserted_executed_block(
        &self,
        block: BuiltPayloadExecutedBlock<EthPrimitives>,
    ) -> ProviderResult<ExecutedBlock<EthPrimitives>> {
        self.inner.on_inserted_executed_block(block)
    }

    fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<EthPrimitives>) {
        self.inner.on_canonical_head_changed(hash, state);
    }

    fn payload_builder_resources(
        &self,
        parent_hash: B256,
        parent_header: &Header,
        timestamp: u64,
        state: &mut EngineApiTreeState<EthPrimitives>,
    ) -> PayloadBuilderResources {
        self.inner
            .payload_builder_resources(parent_hash, parent_header, timestamp, state)
    }
}

impl<V: WaitForCaches> WaitForCaches for ArkivEngineValidator<V> {
    fn wait_for_caches(&self) -> CacheWaitDurations {
        self.inner.wait_for_caches()
    }
}

/// Refuses blocks and build requests that carry consensus-layer withdrawals.
///
/// A withdrawal credits an account outside the EVM: reth's block executor
/// applies it straight to its own `State`, after transaction execution and
/// without passing through Arkiv's EVM. Arkiv's balances live in GolemDB and
/// are written only by that EVM, so since accounts moved there (#154) a
/// withdrawal would credit reth's copy and leave the real balance untouched —
/// the two would disagree, silently, forever.
///
/// Arkiv has no staking, so no honest withdrawal can arrive. Refusing is
/// therefore free, and it is the decision recorded as #145's third open
/// question. The alternative — routing withdrawals into the store — is real
/// work for a feature the chain does not have.
///
/// Both directions are covered:
///
/// * **Inbound**, a payload from the consensus layer, in
///   [`convert_payload_to_block`](PayloadValidator::convert_payload_to_block).
///   Every path to a block runs through it, including
///   `ensure_well_formed_payload`, whose default calls it.
/// * **Outbound**, payload attributes asking us to *build* such a block, in
///   [`validate_payload_attributes_against_header`](PayloadValidator::validate_payload_attributes_against_header).
///   Without this the node would build a block it then rejects itself.
#[derive(Debug, Clone)]
pub struct ArkivPayloadValidator<V> {
    inner: V,
}

impl<V> ArkivPayloadValidator<V> {
    pub const fn new(inner: V) -> Self {
        Self { inner }
    }
}

impl<V, Types> PayloadValidator<Types> for ArkivPayloadValidator<V>
where
    Types: PayloadTypes<ExecutionData = ExecutionData, PayloadAttributes = EthPayloadAttributes>,
    V: PayloadValidator<Types, Block = Block>,
{
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let block = self.inner.convert_payload_to_block(payload)?;
        if let Some(count) =
            withdrawal_count(block.body().withdrawals.as_deref().map(Vec::as_slice))
        {
            return Err(NewPayloadError::other(ArkivWithdrawalsUnsupported(count)));
        }
        Ok(block)
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &Types::PayloadAttributes,
        header: &<Self::Block as reth_ethereum::primitives::Block>::Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if let Some(count) = withdrawal_count(attr.withdrawals.as_deref()) {
            return Err(InvalidPayloadAttributesError::InvalidParams(Box::new(
                ArkivWithdrawalsUnsupported(count),
            )));
        }
        self.inner
            .validate_payload_attributes_against_header(attr, header)
    }
}

impl<V, Types> EngineApiValidator<Types> for ArkivPayloadValidator<V>
where
    Types: PayloadTypes<ExecutionData = ExecutionData, PayloadAttributes = EthPayloadAttributes>,
    V: EngineApiValidator<Types> + PayloadValidator<Types, Block = Block>,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, Types::ExecutionData, EthPayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        self.inner
            .validate_version_specific_fields(version, payload_or_attrs)
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &EthPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        self.inner
            .ensure_well_formed_attributes(version, attributes)
    }
}

/// `Some(n)` when a withdrawal list is present and non-empty. An empty list is
/// what every post-Shanghai block on a chain without staking carries, so it is
/// not an error; `None` is the pre-Shanghai shape.
fn withdrawal_count<T>(withdrawals: Option<&[T]>) -> Option<usize> {
    withdrawals.map(<[T]>::len).filter(|count| *count > 0)
}

/// The refusal itself, so the reason reaches the consensus layer's logs rather
/// than just a rejected hash.
#[derive(Debug)]
struct ArkivWithdrawalsUnsupported(usize);

impl std::fmt::Display for ArkivWithdrawalsUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "arkiv does not support consensus-layer withdrawals, and this payload carries {}. \
             Arkiv balances live in GolemDB and are written only through its executor, which a \
             withdrawal bypasses (see #145).",
            self.0
        )
    }
}

impl std::error::Error for ArkivWithdrawalsUnsupported {}

/// Builds [`ArkivPayloadValidator`] over reth's Ethereum payload validator.
#[derive(Debug, Clone, Default)]
pub struct ArkivPayloadValidatorBuilder;

impl<N> PayloadValidatorBuilder<N> for ArkivPayloadValidatorBuilder
where
    N: FullNodeComponents<Types = ArkivNode>,
    EthereumEngineValidatorBuilder: PayloadValidatorBuilder<N>,
    <EthereumEngineValidatorBuilder as PayloadValidatorBuilder<N>>::Validator:
        PayloadValidator<<ArkivNode as NodeTypes>::Payload, Block = Block>,
{
    type Validator = ArkivPayloadValidator<
        <EthereumEngineValidatorBuilder as PayloadValidatorBuilder<N>>::Validator,
    >;

    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(ArkivPayloadValidator::new(
            EthereumEngineValidatorBuilder::default().build(ctx).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::withdrawal_count;

    #[test]
    fn absent_and_empty_withdrawal_lists_are_not_refused() {
        // Pre-Shanghai blocks have no list; post-Shanghai blocks on a chain
        // without staking carry an empty one. Refusing either would halt the
        // chain on every block.
        assert_eq!(withdrawal_count::<()>(None), None);
        assert_eq!(withdrawal_count::<()>(Some(&[])), None);
    }

    #[test]
    fn any_withdrawal_at_all_is_refused() {
        assert_eq!(withdrawal_count(Some(&[()])), Some(1));
        assert_eq!(withdrawal_count(Some(&[(), ()])), Some(2));
    }
}
