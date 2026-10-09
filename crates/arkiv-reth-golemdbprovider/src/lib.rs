//! [`ArkivProvider`]: the provider the node runs on, on its way to GolemDB.
//!
//! reth's [`BlockchainProvider`] answers every storage question the node asks —
//! blocks, headers, receipts, transactions, account state. Arkiv is moving that
//! storage into GolemDB (#145), and the engine will only accept a type that
//! answers all of it, so the move cannot be done one question at a time from
//! the outside. It has to be done from inside a provider of our own.
//!
//! This is that provider, at its starting point: a newtype that forwards every
//! question to reth's. It changes no behaviour, which is the point — it is the
//! baseline the migration moves away from, one trait at a time, with a working
//! node at every step.
//!
//! The delegation is generated from reth's own impls rather than hand-written,
//! so it matches signature for signature. As a trait group moves to GolemDB its
//! `self.inner` forwarding is replaced; when the last one goes, so does the
//! inner provider.
//!
//! [`BlockchainProvider`]: reth_provider::providers::BlockchainProvider

pub mod state;
pub mod state_root;

pub use state::ArkivStateProvider;
pub use state_root::ArkivStateRootStrategy;

use alloy_consensus::transaction::TransactionMeta;
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{Address, B256, BlockHash, BlockNumber, TxHash, TxNumber};
use alloy_rpc_types_engine::ForkchoiceState;
use arkiv_interfaces::store::{CommitId, Store};
use arkiv_reth_statemanager::{GolemAccounts, HostStore, seed_genesis};
use reth_chain_state::{
    CanonicalInMemoryState, CanonicalStateProvider, ExecutedBlock, ForkChoiceNotifications,
    ForkChoiceSubscriptions, PersistedBlockNotifications, PersistedBlockSubscriptions,
};
use reth_chainspec::{ChainInfo, EthChainSpec};
use reth_db_api::models::{AccountBeforeTx, BlockNumberAddress, StoredBlockBodyIndices};
use reth_execution_types::{ExecutionOutcome, RecoveredBlockAndExecutionOutput};
use reth_node_builder::EngineProviderBuilder;
use reth_node_types::{BlockTy, HeaderTy, ReceiptTy, TxTy};
use reth_primitives_traits::{RecoveredBlock, SealedHeader, SealedOrRecoveredBlock, StorageEntry};
use reth_provider::ProviderFactory;
use reth_provider::providers::{
    BlockchainProvider, ProviderNodeTypes, RocksDBProvider, StaticFileProvider,
    StaticFileProviderRWRefMut,
};
use reth_provider::{
    BalProvider, BlockHashReader, BlockIdReader, BlockNumReader, BlockReader, BlockReaderIdExt,
    BlockSource, CanonChainTracker, CanonStateNotifications, CanonStateSubscriptions,
    ChainSpecProvider, ChangeSetReader, DatabaseProviderFactory, HeaderProvider,
    PruneCheckpointReader, ReceiptProvider, ReceiptProviderIdExt, RocksDBProviderFactory,
    StageCheckpointReader, StateProviderBox, StateProviderFactory, StateReader,
    StaticFileProviderFactory, TransactionVariant, TransactionsProvider,
};
use reth_prune_types::{PruneCheckpoint, PruneSegment};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_static_file_types::StaticFileSegment;
use reth_storage_api::{
    BalStoreHandle, BlockBodyIndicesProvider, NodePrimitivesProvider, StateRangeProviderFactory,
    StateRangeView, StorageChangeSetReader,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use std::ops::{RangeBounds, RangeInclusive};
use std::sync::Arc;
use std::time::Instant;

/// The node's provider. Forwards everything to reth's for now; see the module
/// docs for what replaces each group.
#[derive(Debug)]
pub struct ArkivProvider<N: ProviderNodeTypes> {
    inner: BlockchainProvider<N>,
    store: HostStore,
}

impl<N: ProviderNodeTypes> ArkivProvider<N> {
    /// Wrap reth's provider, over the store the answers will come from.
    pub const fn new(inner: BlockchainProvider<N>, store: HostStore) -> Self {
        Self { inner, store }
    }

    /// The provider being delegated to. Shrinks as trait groups move across.
    pub const fn inner(&self) -> &BlockchainProvider<N> {
        &self.inner
    }

    /// The store the migrated traits read from.
    ///
    /// Carried now, used by none of the impls yet: a trait group cannot move
    /// across until the provider can reach the store, and threading it through
    /// the builder and the node is a change worth making on its own rather
    /// than inside the first migration.
    pub const fn store(&self) -> &HostStore {
        &self.store
    }
}

// Derived `Clone` would demand `N: Clone`, which no node types satisfy; the
// inner provider clones regardless.
impl<N: ProviderNodeTypes> Clone for ArkivProvider<N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            store: self.store.clone(),
        }
    }
}

impl<N: ProviderNodeTypes> NodePrimitivesProvider for ArkivProvider<N> {
    type Primitives = N::Primitives;
}

impl<N: ProviderNodeTypes> BalProvider for ArkivProvider<N> {
    fn bal_store(&self) -> &BalStoreHandle {
        self.inner.bal_store()
    }
}

impl<N: ProviderNodeTypes> StateRangeProviderFactory for ArkivProvider<N> {
    fn state_range_provider(&self, state_root: B256) -> ProviderResult<Option<StateRangeView>> {
        self.inner.state_range_provider(state_root)
    }
}

impl<N: ProviderNodeTypes> DatabaseProviderFactory for ArkivProvider<N> {
    type DB = N::DB;
    type Provider = <BlockchainProvider<N> as DatabaseProviderFactory>::Provider;
    type ProviderRW = <BlockchainProvider<N> as DatabaseProviderFactory>::ProviderRW;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        self.inner.database_provider_ro()
    }

    fn database_provider_rw(&self) -> ProviderResult<Self::ProviderRW> {
        self.inner.database_provider_rw()
    }
}

impl<N: ProviderNodeTypes> StaticFileProviderFactory for ArkivProvider<N> {
    fn static_file_provider(&self) -> StaticFileProvider<Self::Primitives> {
        self.inner.static_file_provider()
    }

    fn get_static_file_writer(
        &self,
        block: BlockNumber,
        segment: StaticFileSegment,
    ) -> ProviderResult<StaticFileProviderRWRefMut<'_, Self::Primitives>> {
        self.inner.get_static_file_writer(block, segment)
    }
}

impl<N: ProviderNodeTypes> RocksDBProviderFactory for ArkivProvider<N> {
    fn rocksdb_provider(&self) -> RocksDBProvider {
        self.inner.rocksdb_provider()
    }

    fn set_pending_rocksdb_batch(&self, _batch: rocksdb::WriteBatchWithTransaction<true>) {
        self.inner.set_pending_rocksdb_batch(_batch)
    }

    fn commit_pending_rocksdb_batches(&self) -> ProviderResult<()> {
        self.inner.commit_pending_rocksdb_batches()
    }
}

impl<N: ProviderNodeTypes> HeaderProvider for ArkivProvider<N> {
    type Header = HeaderTy<N>;

    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        self.inner.header(block_hash)
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Self::Header>> {
        self.inner.header_by_number(num)
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        self.inner.headers_range(range)
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.inner.sealed_header(number)
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.inner.sealed_headers_range(range)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.inner.sealed_headers_while(range, predicate)
    }
}

impl<N: ProviderNodeTypes> BlockHashReader for ArkivProvider<N> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.inner.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.inner.canonical_hashes_range(start, end)
    }
}

impl<N: ProviderNodeTypes> BlockNumReader for ArkivProvider<N> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        self.inner.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.inner.best_block_number()
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.inner.last_block_number()
    }

    fn earliest_block_number(&self) -> ProviderResult<BlockNumber> {
        self.inner.earliest_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        self.inner.block_number(hash)
    }
}

impl<N: ProviderNodeTypes> BlockIdReader for ArkivProvider<N> {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.inner.pending_block_num_hash()
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.inner.safe_block_num_hash()
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.inner.finalized_block_num_hash()
    }
}

impl<N: ProviderNodeTypes> BlockReader for ArkivProvider<N> {
    type Block = BlockTy<N>;

    fn find_block_by_hash(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        self.inner.find_block_by_hash(hash, source)
    }

    fn find_sealed_or_recovered_block(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<SealedOrRecoveredBlock<Self::Block>>> {
        self.inner.find_sealed_or_recovered_block(hash, source)
    }

    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        self.inner.block(id)
    }

    fn pending_block(&self) -> ProviderResult<Option<Arc<RecoveredBlock<Self::Block>>>> {
        self.inner.pending_block()
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<RecoveredBlockAndExecutionOutput<Self::Block, Self::Receipt>>> {
        self.inner.pending_block_and_receipts()
    }

    fn recovered_block(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.inner.recovered_block(id, transaction_kind)
    }

    fn sealed_block_with_senders(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.inner.sealed_block_with_senders(id, transaction_kind)
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        self.inner.block_range(range)
    }

    fn block_with_senders_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        self.inner.block_with_senders_range(range)
    }

    fn recovered_block_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        self.inner.recovered_block_range(range)
    }

    fn block_by_transaction_id(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        self.inner.block_by_transaction_id(id)
    }
}

impl<N: ProviderNodeTypes> TransactionsProvider for ArkivProvider<N> {
    type Transaction = TxTy<N>;

    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        self.inner.transaction_id(tx_hash)
    }

    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        self.inner.transaction_by_id(id)
    }

    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.inner.transaction_by_id_unhashed(id)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        self.inner.transaction_by_hash(hash)
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        self.inner.transaction_by_hash_with_meta(tx_hash)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        self.inner.transactions_by_block(id)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        self.inner.transactions_by_block_range(range)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        self.inner.transactions_by_tx_range(range)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        self.inner.senders_by_tx_range(range)
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        self.inner.transaction_sender(id)
    }
}

impl<N: ProviderNodeTypes> ReceiptProvider for ArkivProvider<N> {
    type Receipt = ReceiptTy<N>;

    fn receipt(&self, id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        self.inner.receipt(id)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        self.inner.receipt_by_hash(hash)
    }

    fn receipts_by_block(
        &self,
        block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        self.inner.receipts_by_block(block)
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        self.inner.receipts_by_tx_range(range)
    }

    fn receipts_by_block_range(
        &self,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        self.inner.receipts_by_block_range(block_range)
    }
}

impl<N: ProviderNodeTypes> ReceiptProviderIdExt for ArkivProvider<N> {
    fn receipts_by_block_id(&self, block: BlockId) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        self.inner.receipts_by_block_id(block)
    }
}

impl<N: ProviderNodeTypes> BlockBodyIndicesProvider for ArkivProvider<N> {
    fn block_body_indices(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        self.inner.block_body_indices(number)
    }

    fn block_body_indices_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        self.inner.block_body_indices_range(range)
    }
}

impl<N: ProviderNodeTypes> StageCheckpointReader for ArkivProvider<N> {
    fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        self.inner.get_stage_checkpoint(id)
    }

    fn get_stage_checkpoint_progress(&self, id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        self.inner.get_stage_checkpoint_progress(id)
    }

    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        self.inner.get_all_checkpoints()
    }
}

impl<N: ProviderNodeTypes> PruneCheckpointReader for ArkivProvider<N> {
    fn get_prune_checkpoint(
        &self,
        segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>> {
        self.inner.get_prune_checkpoint(segment)
    }

    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
        self.inner.get_prune_checkpoints()
    }
}

impl<N: ProviderNodeTypes> ChainSpecProvider for ArkivProvider<N> {
    type ChainSpec = N::ChainSpec;

    fn chain_spec(&self) -> Arc<N::ChainSpec> {
        self.inner.chain_spec()
    }
}

impl<N: ProviderNodeTypes> ArkivProvider<N> {
    /// Wrap one of reth's state providers so its accounts come from the store
    /// at `commit`.
    fn with_accounts(
        &self,
        inner: StateProviderBox,
        commit: CommitId,
    ) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(ArkivStateProvider::new(
            inner,
            GolemAccounts::new(self.store.clone(), commit),
        )))
    }

    /// The commit a block hash names, through the commit's host tag.
    ///
    /// A hash the store does not know is `StateForHashNotFound` rather than an
    /// empty state: it means the block is outside the retention window or was
    /// never adopted, and answering "every account is absent" would be a
    /// confident lie about a block we simply cannot see.
    fn commit_of(&self, hash: BlockHash) -> ProviderResult<CommitId> {
        self.store
            .commit_by_tag(hash.0)
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb commit_by_tag: {e:?}"
                )))
            })?
            .ok_or(ProviderError::StateForHashNotFound(hash))
    }

    /// The commit a block number names: reth's hash for that number, then the
    /// tag. Never `commit = number + anything` — see [`state`](crate::state).
    fn commit_of_number(&self, number: BlockNumber) -> ProviderResult<CommitId> {
        let hash = self
            .inner
            .block_hash(number)?
            .ok_or(ProviderError::HeaderNotFound(number.into()))?;
        self.commit_of(hash)
    }
}

impl<N: ProviderNodeTypes> StateProviderFactory for ArkivProvider<N> {
    type Primitives = N::Primitives;

    fn latest(&self) -> ProviderResult<StateProviderBox> {
        self.with_accounts(self.inner.latest()?, self.store.head())
    }

    fn state_with_block_appended(
        &self,
        parent_hash: BlockHash,
        block: ExecutedBlock<N::Primitives>,
    ) -> ProviderResult<StateProviderBox> {
        // The appended block is not committed, so the store's newest account
        // state is still head. Its own writes reach the caller through the
        // `ExecutedBlock` reth layers on top.
        let inner = self.inner.state_with_block_appended(parent_hash, block)?;
        self.with_accounts(inner, self.store.head())
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        let inner = self.inner.state_by_block_number_or_tag(number_or_tag)?;
        let commit = match number_or_tag {
            BlockNumberOrTag::Number(number) => self.commit_of_number(number)?,
            // Every other tag resolves to a block the node considers current.
            // Arkiv commits only blocks it will not reorg, so latest, safe and
            // finalized are all the store's head.
            _ => self.store.head(),
        };
        self.with_accounts(inner, commit)
    }

    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        let inner = self.inner.history_by_block_number(block_number)?;
        self.with_accounts(inner, self.commit_of_number(block_number)?)
    }

    fn history_by_block_hash(&self, block_hash: BlockHash) -> ProviderResult<StateProviderBox> {
        let inner = self.inner.history_by_block_hash(block_hash)?;
        self.with_accounts(inner, self.commit_of(block_hash)?)
    }

    fn state_by_block_hash(&self, hash: BlockHash) -> ProviderResult<StateProviderBox> {
        let inner = self.inner.state_by_block_hash(hash)?;
        self.with_accounts(inner, self.commit_of(hash)?)
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        self.with_accounts(self.inner.pending()?, self.store.head())
    }

    fn pending_state_by_hash(&self, block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        let Some(inner) = self.inner.pending_state_by_hash(block_hash)? else {
            return Ok(None);
        };
        self.with_accounts(inner, self.store.head()).map(Some)
    }

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        let Some(inner) = self.inner.maybe_pending()? else {
            return Ok(None);
        };
        self.with_accounts(inner, self.store.head()).map(Some)
    }
}

impl<N: ProviderNodeTypes> CanonChainTracker for ArkivProvider<N> {
    type Header = HeaderTy<N>;

    fn on_forkchoice_update_received(&self, _update: &ForkchoiceState) {
        self.inner.on_forkchoice_update_received(_update)
    }

    fn last_received_update_timestamp(&self) -> Option<Instant> {
        self.inner.last_received_update_timestamp()
    }

    fn set_canonical_head(&self, header: SealedHeader<Self::Header>) {
        self.inner.set_canonical_head(header)
    }

    fn set_safe(&self, header: SealedHeader<Self::Header>) {
        self.inner.set_safe(header)
    }

    fn set_finalized(&self, header: SealedHeader<Self::Header>) {
        self.inner.set_finalized(header)
    }
}

impl<N: ProviderNodeTypes> CanonicalStateProvider for ArkivProvider<N> {
    type Primitives = N::Primitives;

    fn canonical_in_memory_state(&self) -> CanonicalInMemoryState<Self::Primitives> {
        self.inner.canonical_in_memory_state()
    }
}

impl<N: ProviderNodeTypes> CanonStateSubscriptions for ArkivProvider<N> {
    type Primitives = N::Primitives;

    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<Self::Primitives> {
        self.inner.subscribe_to_canonical_state()
    }
}

impl<N: ProviderNodeTypes> ForkChoiceSubscriptions for ArkivProvider<N> {
    type Header = HeaderTy<N>;

    fn subscribe_safe_block(&self) -> ForkChoiceNotifications<Self::Header> {
        self.inner.subscribe_safe_block()
    }

    fn subscribe_finalized_block(&self) -> ForkChoiceNotifications<Self::Header> {
        self.inner.subscribe_finalized_block()
    }
}

impl<N: ProviderNodeTypes> PersistedBlockSubscriptions for ArkivProvider<N> {
    fn subscribe_persisted_block(&self) -> PersistedBlockNotifications {
        self.inner.subscribe_persisted_block()
    }
}

impl<N: ProviderNodeTypes> StorageChangeSetReader for ArkivProvider<N> {
    fn storage_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        self.inner.storage_changeset(block_number)
    }

    fn get_storage_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
        storage_key: B256,
    ) -> ProviderResult<Option<StorageEntry>> {
        self.inner
            .get_storage_before_block(block_number, address, storage_key)
    }

    fn storage_changesets_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        self.inner.storage_changesets_range(range)
    }
}

impl<N: ProviderNodeTypes> ChangeSetReader for ArkivProvider<N> {
    fn account_block_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<AccountBeforeTx>> {
        self.inner.account_block_changeset(block_number)
    }

    fn get_account_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
    ) -> ProviderResult<Option<AccountBeforeTx>> {
        self.inner.get_account_before_block(block_number, address)
    }

    fn account_changesets_range(
        &self,
        range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumber, AccountBeforeTx)>> {
        self.inner.account_changesets_range(range)
    }
}

impl<N: ProviderNodeTypes> StateReader for ArkivProvider<N> {
    type Receipt = ReceiptTy<N>;

    fn get_state(
        &self,
        block: BlockNumber,
    ) -> ProviderResult<Option<ExecutionOutcome<Self::Receipt>>> {
        self.inner.get_state(block)
    }
}

impl<N: ProviderNodeTypes> BlockReaderIdExt for ArkivProvider<N>
where
    Self: ReceiptProviderIdExt,
{
    fn block_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Block>> {
        self.inner.block_by_id(id)
    }

    fn header_by_number_or_tag(
        &self,
        id: BlockNumberOrTag,
    ) -> ProviderResult<Option<Self::Header>> {
        self.inner.header_by_number_or_tag(id)
    }

    fn sealed_header_by_number_or_tag(
        &self,
        id: BlockNumberOrTag,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.inner.sealed_header_by_number_or_tag(id)
    }

    fn sealed_header_by_id(
        &self,
        id: BlockId,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.inner.sealed_header_by_id(id)
    }

    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Header>> {
        self.inner.header_by_id(id)
    }
}

/// The delegation is complete exactly when this compiles.
///
/// [`FullProvider`] is the bound the node builder puts on `Node::Provider`, so
/// it is the real specification of "answers everything reth's provider does".
/// Thirty-odd traits are easy to be one method short of, and a missing one
/// surfaces as an inscrutable bound failure deep in the builder; here it
/// surfaces as this line.
///
/// It stays as the migration proceeds: moving a trait group to GolemDB must not
/// drop it out of `FullProvider`.
const fn _satisfies_full_provider<N: ProviderNodeTypes>() {
    const fn assert_full<N: ProviderNodeTypes, P: reth_provider::FullProvider<N>>() {}
    assert_full::<N, ArkivProvider<N>>();
}

/// Builds [`ArkivProvider`] for the engine.
///
/// The launcher cannot be handed a provider — one needs a `ProviderFactory`,
/// which needs an open database, which needs the datadir only the launcher
/// has. So it builds one, and this says what to build. Pair it with
/// `NodeBuilder::with_types_and_provider`, which sets `T::Provider` to match.
#[derive(Debug, Clone)]
pub struct ArkivProviderBuilder {
    store: HostStore,
}

impl ArkivProviderBuilder {
    /// Build providers over this store.
    pub const fn new(store: HostStore) -> Self {
        Self { store }
    }
}

impl<N: ProviderNodeTypes> EngineProviderBuilder<N> for ArkivProviderBuilder {
    type Provider = ArkivProvider<N>;

    fn build_provider(self, factory: ProviderFactory<N>) -> eyre::Result<Self::Provider> {
        // The one place that holds both the store and the chain spec, and it
        // runs once before the chain moves. On a store that already has a
        // commit this is a no-op, so a restart costs one `head()`.
        let spec = factory.chain_spec();
        let alloc = spec.genesis().alloc.iter().map(|(address, account)| {
            arkiv_reth_statemanager::GenesisAccount {
                address: *address,
                balance: account.balance,
                nonce: account.nonce.unwrap_or_default(),
            }
        });
        if let Some(commit) = seed_genesis(&self.store, spec.genesis_hash().0, alloc)
            .map_err(|e| eyre::eyre!("seed the Arkiv store with the genesis allocation: {e:?}"))?
        {
            tracing::info!(
                target: "arkiv-reth",
                commit = commit.0,
                accounts = spec.genesis().alloc.len(),
                "genesis allocation written to the Arkiv store",
            );
        }

        Ok(ArkivProvider::new(
            BlockchainProvider::new(factory)?,
            self.store,
        ))
    }
}
