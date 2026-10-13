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

use alloy_consensus::{BlockHeader as _, Sealable as _, transaction::TransactionMeta};
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{Address, B256, BlockHash, BlockNumber, TxHash, TxNumber};
use alloy_rpc_types_engine::ForkchoiceState;
use arkiv_interfaces::store::{
    CommitId, SegmentAddress, SegmentKey, SegmentOrdinal, SegmentRow, Store,
};
use arkiv_reth_statemanager::{GolemAccounts, HostStore, seed_genesis, segments};
use reth_chain_state::{
    CanonicalInMemoryState, CanonicalStateProvider, ExecutedBlock, ForkChoiceNotifications,
    ForkChoiceSubscriptions, PersistedBlockNotifications, PersistedBlockSubscriptions,
};
use reth_chainspec::{ChainInfo, EthChainSpec};
use reth_db_api::models::{AccountBeforeTx, BlockNumberAddress, StoredBlockBodyIndices};
use reth_execution_types::{ExecutionOutcome, RecoveredBlockAndExecutionOutput};
use reth_node_builder::EngineProviderBuilder;
use reth_node_types::{BlockTy, BodyTy, HeaderTy, ReceiptTy, TxTy};
use reth_primitives_traits::{
    BlockBody as _, RecoveredBlock, SealedHeader, SealedOrRecoveredBlock, StorageEntry,
};
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
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// The node's provider. Forwards everything to reth's for now; see the module
/// docs for what replaces each group.
#[derive(Debug)]
pub struct ArkivProvider<N: ProviderNodeTypes> {
    inner: BlockchainProvider<N>,
    store: HostStore,
    /// The block number of the `blocks` segment's first row, once read.
    /// Shared across clones because the provider is cloned per request and
    /// the answer is a property of the store — see
    /// [`blocks_base`](Self::blocks_base).
    blocks_base: Arc<OnceLock<BlockNumber>>,
}

impl<N: ProviderNodeTypes> ArkivProvider<N> {
    /// Wrap reth's provider, over the store the answers will come from.
    pub fn new(inner: BlockchainProvider<N>, store: HostStore) -> Self {
        Self {
            inner,
            store,
            blocks_base: Arc::new(OnceLock::new()),
        }
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
            blocks_base: self.blocks_base.clone(),
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

    /// From the `blocks` segment, falling back to reth for a block the store
    /// does not have.
    ///
    /// The fallback is what makes this safe to land on a running chain: every
    /// block written before this node started appending them is only in reth's
    /// database, and refusing those would break `eth_getBlockByHash` for the
    /// whole history. It goes away with reth's copy, in step 6.
    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        match self.stored_header(block_hash)? {
            Some(header) => Ok(Some(header)),
            None => self.inner.header(block_hash),
        }
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Self::Header>> {
        let Some(hash) = self.block_hash(num)? else {
            return self.inner.header_by_number(num);
        };
        self.header(hash)
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        // Whatever exists in the range, not an error on the first gap: this is
        // a range query, and reth's own implementations answer with the rows
        // they find.
        let mut headers = Vec::new();
        for number in heights(range, self.best_block_number()?) {
            if let Some(header) = self.header_by_number(number)? {
                headers.push(header);
            }
        }
        Ok(headers)
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        // Falls back whenever the store cannot answer, including when reth has
        // no canonical hash for the height yet -- a block being built is in
        // reth's in-memory chain but not in its hash index, and answering
        // `None` there stops the payload builder.
        //
        // Sealed with reth's own hash rather than re-hashing the header: a
        // disagreement between the two is a bug that must surface in reth's
        // own root check, not be papered over here.
        let Some(hash) = self.block_hash(number)? else {
            return self.inner.sealed_header(number);
        };
        match self.stored_header(hash)? {
            Some(header) => Ok(Some(SealedHeader::new(header, hash))),
            None => self.inner.sealed_header(number),
        }
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        let mut headers = Vec::new();
        for number in heights(range, self.best_block_number()?) {
            if let Some(header) = self.sealed_header(number)? {
                headers.push(header);
            }
        }
        Ok(headers)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        mut predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        // Stops at the first height with no header and at the first the
        // predicate rejects, which is what reth's implementations do.
        let mut headers = Vec::new();
        for number in heights(range, self.best_block_number()?) {
            let Some(header) = self.sealed_header(number)? else {
                break;
            };
            if !predicate(&header) {
                break;
            }
            headers.push(header);
        }
        Ok(headers)
    }
}

/// The first and last transaction number a range names, with `None` for an
/// unbounded end.
///
/// Unlike [`heights`], an unbounded end is not resolved to a tip: there is no
/// cheap "last transaction number" to resolve it to, and none is needed — a
/// caller walking the segment stops at the first ordinal with no row, which is
/// the end of the segment by definition.
fn tx_bounds(range: &impl RangeBounds<TxNumber>) -> (TxNumber, Option<TxNumber>) {
    let start = match range.start_bound() {
        core::ops::Bound::Included(first) => *first,
        core::ops::Bound::Excluded(before) => before.saturating_add(1),
        core::ops::Bound::Unbounded => 0,
    };
    let last = match range.end_bound() {
        core::ops::Bound::Included(last) => Some(*last),
        core::ops::Bound::Excluded(after) => Some(after.saturating_sub(1)),
        core::ops::Bound::Unbounded => None,
    };
    (start, last)
}

/// The heights a `RangeBounds<BlockNumber>` names, with an unbounded end
/// meaning "up to the tip".
fn heights(
    range: impl RangeBounds<BlockNumber>,
    tip: BlockNumber,
) -> impl Iterator<Item = BlockNumber> {
    let start = match range.start_bound() {
        core::ops::Bound::Included(first) => *first,
        core::ops::Bound::Excluded(before) => before.saturating_add(1),
        core::ops::Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        core::ops::Bound::Included(last) => *last,
        core::ops::Bound::Excluded(after) => after.saturating_sub(1),
        core::ops::Bound::Unbounded => tip,
    };
    start..=end
}

impl<N: ProviderNodeTypes> BlockHashReader for ArkivProvider<N> {
    /// From the `blocks` segment by ordinal, falling back to reth.
    ///
    /// This is the keystone of the height side of the tier: `header_by_number`,
    /// `sealed_header`, `block_hash_of` and `commit_of_number` all ask it for
    /// the hash they then look a block up by, so they follow it across without
    /// changing.
    ///
    /// The fallback covers both ends — blocks older than the segment's first
    /// row, which exist only in reth's database, and a height above its last,
    /// which is a block reth is still building.
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        match self.stored_at_height(number)? {
            Some((hash, _)) => Ok(Some(hash)),
            None => self.inner.block_hash(number),
        }
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        // Per height through `block_hash`, so each one takes the tier or the
        // fallback on its own merits. Whatever exists in the range rather than
        // an error on the first gap, which is what reth's own answer is.
        let mut hashes = Vec::new();
        for number in start..end {
            if let Some(hash) = self.block_hash(number)? {
                hashes.push(hash);
            }
        }
        Ok(hashes)
    }
}

/// The tier answers where a block *is*; reth answers where the chain *is*.
///
/// `block_number` is storage — a hash either names a stored row or it does
/// not. The four heights below are live chain state: reth's canonical tip
/// moves with blocks still in memory, ahead of anything committed, and the
/// engine and the payload builder read them constantly while building on top.
/// Answering them from the tier would report a node permanently a block or
/// two behind itself, which is the same over-reach that routing number-ids
/// through the tier caused in #166.
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

    /// From the named `blocks` row — the hash is the row's name, so this is a
    /// point read and needs no index. Falls back to reth for a block written
    /// before this node started appending them.
    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        match self.stored_header(hash)? {
            Some(header) => Ok(Some(header.number())),
            None => self.inner.block_number(hash),
        }
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

    /// The tier answers for a block the caller insists is canonical; anything
    /// that may want a pending block goes to reth first.
    ///
    /// A pending block is not committed, so it is not in the tier at all. For
    /// `BlockSource::Any` reth documents Pending before Database, and asking
    /// the tier first would answer with a canonical block where reth would
    /// have returned the pending one — so for `Any` the tier is only the
    /// fallback.
    fn find_block_by_hash(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        if matches!(source, BlockSource::Canonical) {
            if let Some(block) = self.assembled_block(hash.into())? {
                return Ok(Some(block));
            }
            return self.inner.find_block_by_hash(hash, source);
        }
        match self.inner.find_block_by_hash(hash, source)? {
            Some(block) => Ok(Some(block)),
            None if source.is_canonical() => self.assembled_block(hash.into()),
            None => Ok(None),
        }
    }

    fn find_sealed_or_recovered_block(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<SealedOrRecoveredBlock<Self::Block>>> {
        let from_tier = |provider: &Self| -> ProviderResult<_> {
            Ok(provider
                .recovered_from_tier(hash.into())?
                .map(|block| SealedOrRecoveredBlock::Recovered(Arc::new(block))))
        };
        if matches!(source, BlockSource::Canonical) {
            if let Some(block) = from_tier(self)? {
                return Ok(Some(block));
            }
            return self.inner.find_sealed_or_recovered_block(hash, source);
        }
        match self.inner.find_sealed_or_recovered_block(hash, source)? {
            Some(block) => Ok(Some(block)),
            None if source.is_canonical() => from_tier(self),
            None => Ok(None),
        }
    }

    /// Header from the `blocks` segment, body built from the `transactions`
    /// one — see [`Self::assembled_block`] for how, and for what is checked.
    ///
    /// Falls back to reth for any block the tier cannot produce, including one
    /// whose assembled body does not match its stored header.
    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        match self.assembled_block(id)? {
            Some(block) => Ok(Some(block)),
            None => self.inner.block(id),
        }
    }

    fn pending_block(&self) -> ProviderResult<Option<Arc<RecoveredBlock<Self::Block>>>> {
        self.inner.pending_block()
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<RecoveredBlockAndExecutionOutput<Self::Block, Self::Receipt>>> {
        self.inner.pending_block_and_receipts()
    }

    /// From the tier, with senders taken from the transaction rows rather
    /// than recovered from signatures.
    ///
    /// `transaction_kind` is ignored. It chooses whether reth recovers
    /// transaction hashes eagerly, which is a cost its own storage pays; here
    /// the hashes are the row names and are already known either way.
    fn recovered_block(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        match self.recovered_from_tier(id)? {
            Some(block) => Ok(Some(block)),
            None => self.inner.recovered_block(id, transaction_kind),
        }
    }

    fn sealed_block_with_senders(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        match self.recovered_from_tier(id)? {
            Some(block) => Ok(Some(block)),
            None => self.inner.sealed_block_with_senders(id, transaction_kind),
        }
    }

    /// Whatever the range holds, like every other range here: a gap ends the
    /// walk rather than failing it.
    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        let mut blocks = Vec::new();
        for number in range {
            let Some(block) = self.block(number.into())? else {
                break;
            };
            blocks.push(block);
        }
        Ok(blocks)
    }

    fn block_with_senders_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        self.recovered_block_range(range)
    }

    fn recovered_block_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        let mut blocks = Vec::new();
        for number in range {
            let Some(block) = self.recovered_block(number.into(), TransactionVariant::default())?
            else {
                break;
            };
            blocks.push(block);
        }
        Ok(blocks)
    }

    fn block_by_transaction_id(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        self.inner.block_by_transaction_id(id)
    }
}

impl<N: ProviderNodeTypes> TransactionsProvider for ArkivProvider<N> {
    type Transaction = TxTy<N>;

    /// The ordinal the row named by this hash sits at — a name lookup, which
    /// is all a transaction number is here.
    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.transaction_id(tx_hash);
        }
        match self
            .store
            .segment_ordinal_of(segments::TRANSACTIONS, SegmentKey(tx_hash.0))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_ordinal_of(transactions): {e:?}"
                )))
            })? {
            Some(ordinal) => Ok(Some(ordinal)),
            None => self.inner.transaction_id(tx_hash),
        }
    }

    /// By ordinal, which is what the number is.
    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.transaction_by_id(id);
        }
        match self.transaction_row_at(id)? {
            Some((transaction, _, _)) => Ok(Some(transaction)),
            None => self.inner.transaction_by_id(id),
        }
    }

    /// The same read as [`Self::transaction_by_id`].
    ///
    /// reth separates the two so a caller can skip hashing the transaction.
    /// Here there is nothing to skip: the row decodes to the node's own
    /// transaction type either way, and whether that type caches its hash is
    /// the type's business, not this provider's.
    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.transaction_by_id(id)
    }

    /// From the `transactions` segment, where the row's name is the hash.
    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        match self.stored_transaction(hash)? {
            Some((transaction, _, _)) => Ok(Some(transaction)),
            None => self.inner.transaction_by_hash(hash),
        }
    }

    /// The transaction plus where it sat: its block, its index in that block,
    /// and the fee fields a receipt is read against.
    ///
    /// The block comes from the row itself rather than an index — that is what
    /// the block-hash column is for — and the index from the position of this
    /// hash in the block's own list.
    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        let Some((transaction, _, block_hash)) = self.stored_transaction(tx_hash)? else {
            return self.inner.transaction_by_hash_with_meta(tx_hash);
        };
        let (Some(header), Some(hashes)) = (
            self.stored_header(block_hash)?,
            self.block_transaction_hashes(block_hash)?,
        ) else {
            // The transaction is stored but its block is not. Rather than
            // invent a position, let reth answer.
            return self.inner.transaction_by_hash_with_meta(tx_hash);
        };
        let Some(index) = hashes.iter().position(|hash| *hash == tx_hash) else {
            return Err(ProviderError::other(std::io::Error::other(format!(
                "transaction {tx_hash} names block {block_hash} but is not in it"
            ))));
        };
        Ok(Some((
            transaction,
            TransactionMeta {
                tx_hash,
                index: index as u64,
                block_hash,
                block_number: header.number(),
                base_fee: header.base_fee_per_gas(),
                excess_blob_gas: header.excess_blob_gas(),
                timestamp: header.timestamp(),
            },
        )))
    }

    /// A block's transactions, read by the names its `blocks` row carries.
    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        let Some(block_hash) = self.block_hash_of(id)? else {
            return self.inner.transactions_by_block(id);
        };
        let Some(hashes) = self.block_transaction_hashes(block_hash)? else {
            return self.inner.transactions_by_block(id);
        };
        let mut transactions = Vec::with_capacity(hashes.len());
        for hash in hashes {
            // A block that names a transaction the store does not have is a
            // half-written block, which adoption is supposed to make
            // impossible. Loud rather than a short list.
            let (transaction, _, _) = self.stored_transaction(hash)?.ok_or_else(|| {
                ProviderError::other(std::io::Error::other(format!(
                    "block {block_hash} names transaction {hash}, which is not stored"
                )))
            })?;
            transactions.push(transaction);
        }
        Ok(Some(transactions))
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        let mut blocks = Vec::new();
        for number in heights(range, self.best_block_number()?) {
            if let Some(transactions) = self.transactions_by_block(number.into())? {
                blocks.push(transactions);
            }
        }
        Ok(blocks)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.transactions_by_tx_range(range);
        }
        let (start, last) = tx_bounds(&range);
        let mut transactions = Vec::new();
        let mut id = start;
        while last.is_none_or(|last| id <= last) {
            let Some((transaction, _, _)) = self.transaction_row_at(id)? else {
                break;
            };
            transactions.push(transaction);
            id += 1;
        }
        if transactions.is_empty() {
            // The tier does not reach this range; defer rather than report
            // that it holds nothing.
            return self.inner.transactions_by_tx_range(range);
        }
        Ok(transactions)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.senders_by_tx_range(range);
        }
        let (start, last) = tx_bounds(&range);
        let mut senders = Vec::new();
        let mut id = start;
        while last.is_none_or(|last| id <= last) {
            let Some((_, sender, _)) = self.transaction_row_at(id)? else {
                break;
            };
            senders.push(sender);
            id += 1;
        }
        if senders.is_empty() {
            return self.inner.senders_by_tx_range(range);
        }
        Ok(senders)
    }

    /// The sender rides on the transaction's own row, which is why it is
    /// stored there — recovering it from the signature again would cost more
    /// than the read.
    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.transaction_sender(id);
        }
        match self.transaction_row_at(id)? {
            Some((_, sender, _)) => Ok(Some(sender)),
            None => self.inner.transaction_sender(id),
        }
    }
}

impl<N: ProviderNodeTypes> ReceiptProvider for ArkivProvider<N> {
    type Receipt = ReceiptTy<N>;

    /// By ordinal: a receipt sits at the same ordinal as its transaction.
    fn receipt(&self, id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.receipt(id);
        }
        match self.receipt_row_at(id)? {
            Some(receipt) => Ok(Some(receipt)),
            None => self.inner.receipt(id),
        }
    }

    /// From the `receipts` segment, where a receipt's row is named by its
    /// transaction's hash.
    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        match self.stored_receipt(hash)? {
            Some(receipt) => Ok(Some(receipt)),
            None => self.inner.receipt_by_hash(hash),
        }
    }

    /// A block's receipts, in transaction order, read by the same names.
    fn receipts_by_block(
        &self,
        block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        let Some(block_hash) = self.block_hash_of(block)? else {
            return self.inner.receipts_by_block(block);
        };
        let Some(hashes) = self.block_transaction_hashes(block_hash)? else {
            return self.inner.receipts_by_block(block);
        };
        let mut receipts = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let receipt = self.stored_receipt(hash)?.ok_or_else(|| {
                ProviderError::other(std::io::Error::other(format!(
                    "block {block_hash} names transaction {hash}, whose receipt is not stored"
                )))
            })?;
            receipts.push(receipt);
        }
        Ok(Some(receipts))
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.receipts_by_tx_range(range);
        }
        let (start, last) = tx_bounds(&range);
        let mut receipts = Vec::new();
        let mut id = start;
        while last.is_none_or(|last| id <= last) {
            let Some(receipt) = self.receipt_row_at(id)? else {
                break;
            };
            receipts.push(receipt);
            id += 1;
        }
        if receipts.is_empty() {
            return self.inner.receipts_by_tx_range(range);
        }
        Ok(receipts)
    }

    fn receipts_by_block_range(
        &self,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        let mut blocks = Vec::new();
        for number in block_range {
            if let Some(receipts) = self.receipts_by_block(number.into())? {
                blocks.push(receipts);
            }
        }
        Ok(blocks)
    }
}

impl<N: ProviderNodeTypes> ReceiptProviderIdExt for ArkivProvider<N> {
    /// Resolved to a block and then read from the tier.
    ///
    /// Only a hash or a plain number resolves here. The tags — `latest`,
    /// `safe`, `finalized`, `pending`, `earliest` — are forkchoice state
    /// rather than block data, and reth is what tracks them, so those stay
    /// with it.
    fn receipts_by_block_id(&self, block: BlockId) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        if let Some(id) = resolve_block_id(block)
            && let Some(receipts) = self.receipts_by_block(id)?
        {
            return Ok(Some(receipts));
        }
        self.inner.receipts_by_block_id(block)
    }
}

impl<N: ProviderNodeTypes> BlockBodyIndicesProvider for ArkivProvider<N> {
    /// Where a block's transactions sit in the numbering, which is exactly the
    /// ordinal range its commit appended to `transactions`.
    ///
    /// `first_tx_num` and `tx_count` are the range's start and length, so this
    /// needs no index and no walk — the commit already recorded it. An empty
    /// block answers `end..end`, which is why
    /// [`Store::segment_range_of`](arkiv_interfaces::store::Store::segment_range_of)
    /// has to place an empty range rather than return `0..0`.
    ///
    /// The commit is resolved through the tag directly rather than through
    /// `commit_of`, which refuses anything but the head: that guard is about
    /// reading *state* at an old commit, which GolemDB cannot do. What a
    /// commit appended to a segment is a recorded fact about the past and is
    /// served for any retained commit.
    fn block_body_indices(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        if !self.ordinals_are_tx_numbers()? {
            return self.inner.block_body_indices(number);
        }
        let Some(hash) = self.block_hash(number)? else {
            return self.inner.block_body_indices(number);
        };
        let found = self.store.commit_by_tag(hash.0).map_err(|e| {
            ProviderError::other(std::io::Error::other(format!(
                "golemdb commit_by_tag: {e:?}"
            )))
        })?;
        let Some(commit) = found else {
            return self.inner.block_body_indices(number);
        };
        let range = self
            .store
            .segment_range_of(segments::TRANSACTIONS, commit)
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_range_of(transactions): {e:?}"
                )))
            })?;
        Ok(Some(StoredBlockBodyIndices {
            first_tx_num: range.start,
            tx_count: range.end - range.start,
        }))
    }

    fn block_body_indices_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        let mut indices = Vec::new();
        for number in range.clone() {
            let Some(found) = self.block_body_indices(number)? else {
                break;
            };
            indices.push(found);
        }
        if indices.is_empty() {
            return self.inner.block_body_indices_range(range);
        }
        Ok(indices)
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
    ///
    /// # Only the head commit can be read
    ///
    /// A commit that resolves to anything other than the store's head is
    /// refused. GolemDB serves reads at its current head only — reading at a
    /// past commit is not supported and is not planned (#145) — so a provider
    /// built on an older commit would silently answer from head while claiming
    /// to speak for an earlier block. That is the one failure mode worth
    /// refusing loudly: a wrong balance is indistinguishable from a right one.
    ///
    /// This is what makes a live Arkiv node a non-archive node. State history
    /// still exists in the store and is reachable by rewinding a *copy*, which
    /// is a one-way operation and so cannot be done by a node serving traffic.
    fn commit_of(&self, hash: BlockHash) -> ProviderResult<CommitId> {
        let commit = self
            .store
            .commit_by_tag(hash.0)
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb commit_by_tag: {e:?}"
                )))
            })?
            .ok_or(ProviderError::StateForHashNotFound(hash))?;
        head_only(commit, self.store.head())?;
        Ok(commit)
    }

    /// The commit a block number names: reth's hash for that number, then the
    /// tag. Never `commit = number + anything` — see [`state`](crate::state).
    fn commit_of_number(&self, number: BlockNumber) -> ProviderResult<CommitId> {
        let hash = self
            .block_hash(number)?
            .ok_or(ProviderError::HeaderNotFound(number.into()))?;
        self.commit_of(hash)
    }

    /// The stored block a hash names, from the `blocks` segment.
    ///
    /// A direct read: the row's name *is* the block hash, so there is no index
    /// to consult and no commit to resolve first. `None` means the store has
    /// no such block, which includes every block written before this node
    /// started appending them.
    fn stored_block_row(&self, hash: BlockHash) -> ProviderResult<Option<SegmentRow>> {
        self.store
            .segment_get(segments::BLOCKS, SegmentAddress::Key(SegmentKey(hash.0)))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(blocks): {e:?}"
                )))
            })
    }

    /// A whole block assembled from the tier, or `None` if it cannot be.
    ///
    /// # Building a body without pinning the block type
    ///
    /// `Block::new(header, body)` needs a `Block::Body`, and reth's
    /// `BlockBody` trait has no constructor — it converts a body *into* an
    /// Ethereum body and never the other way. But it does require RLP
    /// `Encodable` and `Decodable`, so the body can be built concretely and
    /// then round-tripped into the node's own type. RLP is the consensus wire
    /// format for a body, so this is the same path a body takes arriving from
    /// a peer; it costs one encode and one decode per block read.
    ///
    /// # The parts of a body the tier does not store
    ///
    /// A body is its transactions, its ommers and its withdrawals. Only the
    /// transactions are stored: Arkiv is post-merge so there are no ommers,
    /// and it has no staking so there are no withdrawals.
    ///
    /// That is an assumption, so it is checked rather than trusted — the
    /// assembled body is accepted only if it hashes to the `ommers_hash`,
    /// `transactions_root` and `withdrawals_root` the stored header already
    /// commits to. A block that fails is served from reth instead, so a wrong
    /// assumption costs a fallback rather than a wrong block.
    fn assembled_block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<BlockTy<N>>> {
        let Some(block_hash) = self.block_hash_of(id)? else {
            return Ok(None);
        };
        let (Some(header), Some(transactions)) = (
            self.stored_header(block_hash)?,
            self.transactions_by_block(block_hash.into())?,
        ) else {
            return Ok(None);
        };

        // Withdrawals follow the header: a header with no withdrawals root
        // predates them, one with a root has a list, which Arkiv keeps empty.
        let withdrawals = header
            .withdrawals_root()
            .map(|_| alloy_eips::eip4895::Withdrawals::default());
        let body = alloy_consensus::BlockBody::<TxTy<N>, HeaderTy<N>> {
            transactions,
            ommers: Vec::new(),
            withdrawals,
        };

        if body.calculate_ommers_root() != header.ommers_hash()
            || body.calculate_tx_root() != header.transactions_root()
            || body.calculate_withdrawals_root() != header.withdrawals_root()
        {
            // The stored header does not agree that this is its body, so the
            // empty-ommers/empty-withdrawals assumption does not hold here.
            return Ok(None);
        }

        let body: BodyTy<N> = alloy_rlp::Decodable::decode(
            &mut alloy_rlp::encode(&body).as_slice(),
        )
        .map_err(|e| {
            ProviderError::other(std::io::Error::other(format!(
                "assembled block body does not round-trip through RLP: {e}"
            )))
        })?;
        Ok(Some(reth_primitives_traits::Block::new(header, body)))
    }

    /// A whole block plus the senders of its transactions, both from the tier.
    ///
    /// The senders ride on the transaction rows, so recovering a block costs
    /// no signature recovery — which is the reason they are stored at all.
    fn recovered_from_tier(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<RecoveredBlock<BlockTy<N>>>> {
        let Some(block_hash) = self.block_hash_of(id)? else {
            return Ok(None);
        };
        let Some(block) = self.assembled_block(block_hash.into())? else {
            return Ok(None);
        };
        let Some(hashes) = self.block_transaction_hashes(block_hash)? else {
            return Ok(None);
        };
        let mut senders = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let (_, sender, _) = self.stored_transaction(hash)?.ok_or_else(|| {
                ProviderError::other(std::io::Error::other(format!(
                    "block {block_hash} names transaction {hash}, which is not stored"
                )))
            })?;
            senders.push(sender);
        }
        Ok(Some(RecoveredBlock::new(block, senders, block_hash)))
    }

    /// The transaction hashes a block contains, in order, from its `blocks`
    /// row. `None` if the store has no such block.
    ///
    /// These are what turn every block-addressed read into a set of point
    /// reads by name: the *n*th hash names the *n*th transaction and the *n*th
    /// receipt.
    fn block_transaction_hashes(&self, hash: BlockHash) -> ProviderResult<Option<Vec<B256>>> {
        let Some(row) = self.stored_block_row(hash)? else {
            return Ok(None);
        };
        Ok(Some(
            segments::decode_block(&row)
                .map_err(|e| ProviderError::other(std::io::Error::other(e)))?
                .transactions,
        ))
    }

    /// Resolve a block id to the hash the segments are named by.
    ///
    /// A number goes through reth, which still owns the number-to-hash
    /// mapping — see [`Self::stored_header`].
    fn block_hash_of(&self, id: BlockHashOrNumber) -> ProviderResult<Option<BlockHash>> {
        match id {
            BlockHashOrNumber::Hash(hash) => Ok(Some(hash)),
            BlockHashOrNumber::Number(number) => self.block_hash(number),
        }
    }

    /// One transaction by its hash, with its sender and its block.
    ///
    /// Decoded generically for the same reason headers are: pinning the
    /// transaction type here would spread the pin across every impl that
    /// `TransactionsProvider` is a supertrait of.
    fn stored_transaction(
        &self,
        hash: TxHash,
    ) -> ProviderResult<Option<(TxTy<N>, Address, BlockHash)>> {
        let row = self
            .store
            .segment_get(
                segments::TRANSACTIONS,
                SegmentAddress::Key(SegmentKey(hash.0)),
            )
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(transactions): {e:?}"
                )))
            })?;
        row.as_deref().map(Self::decode_transaction_row).transpose()
    }

    /// The three columns of a `transactions` row: the transaction, its sender
    /// and the block it was in.
    fn decode_transaction_row(row: &[Vec<u8>]) -> ProviderResult<(TxTy<N>, Address, BlockHash)> {
        let [transaction, sender, block_hash] = row else {
            return Err(ProviderError::other(std::io::Error::other(
                "a transactions row has three columns",
            )));
        };
        let transaction = alloy_eips::eip2718::Decodable2718::decode_2718(
            &mut transaction.as_slice(),
        )
        .map_err(|e| {
            ProviderError::other(std::io::Error::other(format!(
                "transactions row is not valid EIP-2718: {e}"
            )))
        })?;
        let sender = Address::try_from(sender.as_slice()).map_err(|_| {
            ProviderError::other(std::io::Error::other("sender column is not 20 bytes"))
        })?;
        let block_hash = <[u8; 32]>::try_from(block_hash.as_slice())
            .map(BlockHash::from)
            .map_err(|_| {
                ProviderError::other(std::io::Error::other("block hash column is not 32 bytes"))
            })?;
        Ok((transaction, sender, block_hash))
    }

    /// One receipt by its transaction's hash, from the `receipts` segment.
    fn stored_receipt(&self, hash: TxHash) -> ProviderResult<Option<ReceiptTy<N>>> {
        let row = self
            .store
            .segment_get(segments::RECEIPTS, SegmentAddress::Key(SegmentKey(hash.0)))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(receipts): {e:?}"
                )))
            })?;
        row.as_deref().map(Self::decode_receipt_row).transpose()
    }

    /// The single column of a `receipts` row.
    fn decode_receipt_row(row: &[Vec<u8>]) -> ProviderResult<ReceiptTy<N>> {
        let [receipt] = row else {
            return Err(ProviderError::other(std::io::Error::other(
                "a receipts row has one column",
            )));
        };
        // Decoded generically, like headers: plain RLP, which reth's receipt
        // trait guarantees in both directions. EIP-2718 it does not.
        alloy_rlp::Decodable::decode(&mut receipt.as_slice()).map_err(|e| {
            ProviderError::other(std::io::Error::other(format!(
                "receipts row is not RLP: {e}"
            )))
        })
    }

    /// The header a hash names, decoded as this node's own header type.
    ///
    /// Decoded generically rather than as a concrete `alloy_consensus::Header`:
    /// pinning the type here would force the same pin onto every impl that
    /// `HeaderProvider` is a supertrait of, which is most of this file. The
    /// row is consensus RLP, so any header type that round-trips through RLP
    /// reads it.
    fn stored_header(&self, hash: BlockHash) -> ProviderResult<Option<HeaderTy<N>>> {
        let Some(row) = self.stored_block_row(hash)? else {
            return Ok(None);
        };
        Self::header_of_row(&row).map(Some)
    }

    /// The header column of a `blocks` row, decoded.
    fn header_of_row(row: &SegmentRow) -> ProviderResult<HeaderTy<N>> {
        let [header, _] = row.as_slice() else {
            return Err(ProviderError::other(std::io::Error::other(
                "a blocks row has two columns",
            )));
        };
        alloy_rlp::Decodable::decode(&mut header.as_slice()).map_err(|e| {
            ProviderError::other(std::io::Error::other(format!(
                "blocks row header is not RLP: {e}"
            )))
        })
    }

    /// The `blocks` row at `ordinal`, or `None` past the end of the segment.
    fn block_row_at(&self, ordinal: SegmentOrdinal) -> ProviderResult<Option<SegmentRow>> {
        self.store
            .segment_get(segments::BLOCKS, SegmentAddress::Ordinal(ordinal))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(blocks, #{ordinal}): {e:?}"
                )))
            })
    }

    /// The block number of the `blocks` segment's first row.
    ///
    /// One row is appended per committed block and ordinals are dense, so a
    /// height is `ordinal + base` and an ordinal is `height - base`. The base
    /// is not necessarily genesis or one: a node that began appending partway
    /// through a chain has its own first block sitting at ordinal 0, and every
    /// block before it exists only in reth's database.
    ///
    /// Read once and kept, because ordinal 0 cannot change — a row provably
    /// cannot move, which is why appends require a sealed branch. An empty
    /// segment answers `None` and is not cached, so the first block to arrive
    /// sets the base.
    fn blocks_base(&self) -> ProviderResult<Option<BlockNumber>> {
        if let Some(base) = self.blocks_base.get() {
            return Ok(Some(*base));
        }
        let Some(row) = self.block_row_at(0)? else {
            return Ok(None);
        };
        let base = Self::header_of_row(&row)?.number();
        let _ = self.blocks_base.set(base);
        Ok(Some(base))
    }

    /// Whether the tier's ordinals *are* reth's transaction numbers.
    ///
    /// reth numbers every transaction in the chain from zero, in block order,
    /// and the `transactions` segment appends in exactly that order — one row
    /// per transaction, enforced against the sender and receipt counts. So the
    /// two numberings coincide, but only if the segment begins at the chain's
    /// first block. A node that started appending partway through has ordinal
    /// 0 sitting on some later transaction, and serving `transaction_by_id`
    /// from it would hand back a *different* transaction under the same
    /// number — the one failure mode here that cannot be noticed afterwards.
    ///
    /// So it is a precondition and it is checked. When it does not hold, every
    /// ordinal-addressed read falls back to reth, which still holds the
    /// numbering it assigned. Genesis carries no transactions, so the first
    /// block to append is the one after it.
    fn ordinals_are_tx_numbers(&self) -> ProviderResult<bool> {
        let Some(base) = self.blocks_base()? else {
            return Ok(false);
        };
        let genesis = self.chain_spec().genesis().number.unwrap_or_default();
        Ok(base == genesis + 1)
    }

    /// A `transactions` row by ordinal, which is reth's `TxNumber` once
    /// [`ordinals_are_tx_numbers`](Self::ordinals_are_tx_numbers) holds.
    fn transaction_row_at(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<(TxTy<N>, Address, BlockHash)>> {
        let row = self
            .store
            .segment_get(segments::TRANSACTIONS, SegmentAddress::Ordinal(id))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(transactions, #{id}): {e:?}"
                )))
            })?;
        row.as_deref().map(Self::decode_transaction_row).transpose()
    }

    /// A `receipts` row by ordinal.
    ///
    /// The same ordinal as the transaction it belongs to: `rows_for_block`
    /// appends one receipt per transaction over the same hash list, so the two
    /// segments carry identical ordinal sequences. That is what makes
    /// `receipt(id)` a direct read rather than a lookup through the block.
    fn receipt_row_at(&self, id: TxNumber) -> ProviderResult<Option<ReceiptTy<N>>> {
        let row = self
            .store
            .segment_get(segments::RECEIPTS, SegmentAddress::Ordinal(id))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(receipts, #{id}): {e:?}"
                )))
            })?;
        row.as_deref().map(Self::decode_receipt_row).transpose()
    }

    /// The hash and header of the block at `number`, from the `blocks`
    /// segment, or `None` if the segment does not reach that height.
    ///
    /// The dense-ordinal assumption is checked rather than trusted: a row that
    /// turns out not to be the height asked for means the mapping is wrong,
    /// and the caller falls back to reth instead of being handed the wrong
    /// block. A gap cannot arise today — every adopted block appends exactly
    /// one row — but this is the one place that would silently serve the
    /// neighbouring block if one ever did.
    fn stored_at_height(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<(BlockHash, HeaderTy<N>)>> {
        let Some(base) = self.blocks_base()? else {
            return Ok(None);
        };
        let Some(ordinal) = number.checked_sub(base) else {
            return Ok(None);
        };
        let Some(row) = self.block_row_at(ordinal)? else {
            return Ok(None);
        };
        let header = Self::header_of_row(&row)?;
        if header.number() != number {
            return Ok(None);
        }
        // The hash is not stored beside the header — it names the row, and a
        // name cannot be read back off one. Re-deriving it is not a second
        // opinion: keccak of the header RLP *is* the block hash, and a
        // disagreement would mean the row could not be found by hash at all,
        // which the store's own conformance covers.
        Ok(Some((header.hash_slow(), header)))
    }
}

/// Refuse a commit that is not the store's head — see [`ArkivProvider::commit_of`].
///
/// A free function rather than a method so it can be tested without standing up
/// a whole `BlockchainProvider`; it needs nothing but the two numbers.
fn head_only(commit: CommitId, head: CommitId) -> ProviderResult<()> {
    if commit == head {
        return Ok(());
    }
    Err(ProviderError::other(std::io::Error::other(format!(
        "arkiv serves state at the chain head only: asked for commit {}, head is {}. GolemDB \
         cannot read at a past commit, so a live node has no historical state view (see #145).",
        commit.0, head.0
    ))))
}

#[cfg(test)]
mod tx_bounds_tests {
    use super::tx_bounds;

    #[test]
    fn a_half_open_range_excludes_its_end() {
        assert_eq!(tx_bounds(&(4u64..7)), (4, Some(6)));
    }

    #[test]
    fn an_inclusive_range_keeps_its_end() {
        assert_eq!(tx_bounds(&(4u64..=7)), (4, Some(7)));
    }

    #[test]
    fn an_unbounded_end_is_not_resolved_to_a_number() {
        // The caller stops at the first ordinal with no row instead, which is
        // why this is `None` rather than a tip.
        assert_eq!(tx_bounds(&(4u64..)), (4, None));
        assert_eq!(tx_bounds(&(..)), (0, None));
    }

    #[test]
    fn an_empty_range_is_expressed_as_a_last_below_the_start() {
        // `4..4` names nothing, and the loop's `id <= last` sees that.
        assert_eq!(tx_bounds(&(4u64..4)), (4, Some(3)));
    }
}

#[cfg(test)]
mod head_only_tests {
    use super::head_only;
    use arkiv_interfaces::store::CommitId;

    #[test]
    fn the_head_commit_is_served() {
        assert!(head_only(CommitId(7), CommitId(7)).is_ok());
    }

    #[test]
    fn an_older_commit_is_refused_rather_than_answered_from_head() {
        // The whole point: without this the provider would read at head and
        // present the answer as block 6's state.
        let err = head_only(CommitId(6), CommitId(7)).unwrap_err();
        assert!(
            err.to_string().contains("chain head only"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_future_commit_is_refused_too() {
        assert!(head_only(CommitId(8), CommitId(7)).is_err());
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
    // Convenience wrappers over the lookups above. Left delegating they would
    // quietly bypass the tier -- `header_by_id(Hash)` would read reth's header
    // even though the hash names a row we hold -- so the **hash** case is
    // routed through this provider.
    //
    // Only the hash case. A number or a tag goes to reth untouched: these
    // wrappers also resolve `pending`, and reth's answer for a height can be a
    // block it is still building, which is not committed and so is not in the
    // tier. Routing those here turned a pending block into a missing one and
    // stalled the chain.

    fn block_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Block>> {
        match id {
            BlockId::Hash(hash) => self.block(hash.block_hash.into()),
            BlockId::Number(_) => self.inner.block_by_id(id),
        }
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
        match id {
            // Sealed with the hash it was asked for, which is the hash the row
            // is named by.
            BlockId::Hash(hash) => Ok(self
                .header(hash.block_hash)?
                .map(|header| SealedHeader::new(header, hash.block_hash))),
            BlockId::Number(_) => self.inner.sealed_header_by_id(id),
        }
    }

    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Header>> {
        match id {
            BlockId::Hash(hash) => self.header(hash.block_hash),
            BlockId::Number(_) => self.inner.header_by_id(id),
        }
    }
}

/// A [`BlockId`] as a hash or a height, or `None` for a tag that only the
/// canonical-chain tracker can resolve.
const fn resolve_block_id(id: BlockId) -> Option<BlockHashOrNumber> {
    match id {
        BlockId::Hash(hash) => Some(BlockHashOrNumber::Hash(hash.block_hash)),
        BlockId::Number(BlockNumberOrTag::Number(number)) => {
            Some(BlockHashOrNumber::Number(number))
        }
        BlockId::Number(_) => None,
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
