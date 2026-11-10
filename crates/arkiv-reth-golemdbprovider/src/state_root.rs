//! The state root the header carries is GolemDB's, not an MPT root.
//!
//! GolemDB computes its own commitment when a branch seals, so Arkiv has a root
//! already and has no account trie to walk for a second one. This is the
//! validation half of putting that root in the header; the build half is the
//! resolver in `arkiv-reth-payload-builder`.
//!
//! # Why it is a strategy and not a provider method
//!
//! `StateRootProvider::state_root` is handed a `HashedPostState` and asked to
//! compute — the wrong shape, because our root does not come from the post
//! state. `StateRootStrategy` is the seam that runs *after* execution, which is
//! when a seal exists. It is installed once per node through
//! `BasicEngineValidator::with_state_root_strategy`.
//!
//! # What it depends on
//!
//! That GolemDB's root is **stable across re-execution of the same block**. The
//! builder seals one branch and validation re-executes the same block on
//! another; the header only survives if both branches seal to the same root.
//! Nothing here can check that, and it is the one property in the whole
//! migration that Arkiv has not been able to verify locally — see #145.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Transaction};
use alloy_primitives::B256;
use arkiv_reth_statemanager::{BlockSeals, ExecutionKey};
use reth_engine_tree::tree::state_root_strategy::{
    LazyHashedPostState, PreparedStateRootJob, StateRootJob, StateRootJobContext,
    StateRootJobOutcome, StateRootStrategy,
};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use reth_provider::{BlockExecutionOutput, ProviderResult};
use reth_trie::updates::TrieUpdates;

/// Answers engine validation with the root GolemDB sealed.
#[derive(Debug)]
pub struct ArkivStateRootStrategy {
    seals: Arc<BlockSeals>,
}

impl ArkivStateRootStrategy {
    /// Read roots out of `seals`, which the executor writes as it seals.
    pub const fn new(seals: Arc<BlockSeals>) -> Self {
        Self { seals }
    }
}

impl<N, P, Evm> StateRootStrategy<N, P, Evm> for ArkivStateRootStrategy
where
    N: NodePrimitives,
    Evm: ConfigureEvm<Primitives = N>,
{
    fn prepare(
        &self,
        _ctx: StateRootJobContext<'_, N, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<N>> {
        // No streaming capabilities attached: nothing here watches execution.
        // The root exists once the branch seals, which is after execution, so
        // `finish` is the only hook this needs.
        Ok(PreparedStateRootJob::new(
            Box::new(ArkivStateRootJob {
                seals: self.seals.clone(),
            }),
            None,
        ))
    }
}

#[derive(Debug)]
struct ArkivStateRootJob {
    seals: Arc<BlockSeals>,
}

impl<N: NodePrimitives> StateRootJob<N> for ArkivStateRootJob {
    fn name(&self) -> &'static str {
        "arkiv-golemdb"
    }

    fn finish(
        &mut self,
        block: &RecoveredBlock<N::Block>,
        _output: Arc<BlockExecutionOutput<N::Receipt>>,
        _hashed_state: &LazyHashedPostState,
    ) -> ProviderResult<StateRootJobOutcome> {
        let key = ExecutionKey::new(
            block.header().number(),
            block
                .transactions_recovered()
                .map(|tx| (tx.signer().into_array(), tx.nonce()))
                .collect(),
        );

        // Falling back to the header's own root rather than erroring. A block
        // whose execution left no seal is one Arkiv charged nothing for -- an
        // empty block -- and refusing those would stall the chain on the first
        // quiet slot. A block that *did* seal and disagrees still fails, in
        // reth's own root check a moment later, which is where it belongs.
        let state_root = self
            .seals
            .root_of(&key)
            .map_or_else(|| block.header().state_root(), B256::from);

        // No trie updates, ever: Arkiv maintains no account trie, which is the
        // deliberate trade recorded in #145 -- `eth_getProof` cannot be served
        // from stored trie nodes that do not exist.
        Ok(StateRootJobOutcome::new(
            state_root,
            Arc::new(TrieUpdates::default()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Address, Signature};
    use reth_ethereum::tasks::LazyHandle;
    use reth_ethereum_primitives::{Block, BlockBody, EthPrimitives, TransactionSigned};
    use reth_trie::HashedPostState;

    /// Finish an Arkiv state-root job for `block` against `seals`.
    fn finish(
        seals: Arc<BlockSeals>,
        block: &RecoveredBlock<Block>,
    ) -> ProviderResult<StateRootJobOutcome> {
        StateRootJob::<EthPrimitives>::finish(
            &mut ArkivStateRootJob { seals },
            block,
            Arc::new(BlockExecutionOutput::default()),
            &LazyHandle::ready(Arc::new(HashedPostState::default())),
        )
    }

    /// A block at height 1 carrying `transactions`, whose header claims a root
    /// no execution produced.
    fn block(transactions: Vec<TransactionSigned>) -> RecoveredBlock<Block> {
        let senders = vec![Address::repeat_byte(0x11); transactions.len()];
        let header = Header {
            number: 1,
            state_root: B256::repeat_byte(0xab),
            ..Default::default()
        };
        RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody {
                    transactions,
                    ..Default::default()
                },
            },
            senders,
        )
    }

    /// A block with no transactions seals nothing, so there is no root to
    /// check its header against. Answering with the header's own root would
    /// accept whatever it claims.
    #[test]
    fn an_empty_block_without_a_seal_is_refused() {
        let outcome = finish(Arc::new(BlockSeals::new()), &block(Vec::new()));
        assert!(
            outcome.is_err(),
            "an unsealed block's header root was accepted unchecked: {outcome:?}"
        );
    }

    /// A block whose transactions ran but left no seal -- the executor failed
    /// to seal it -- has no root to check either.
    #[test]
    fn a_block_whose_seal_is_missing_is_refused() {
        let tx = TxLegacy::default().into_signed(Signature::test_signature());
        let outcome = finish(Arc::new(BlockSeals::new()), &block(vec![tx.into()]));
        assert!(
            outcome.is_err(),
            "an unsealed block's header root was accepted unchecked: {outcome:?}"
        );
    }
}
