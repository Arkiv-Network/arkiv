//! [`ArkivStateProvider`] — the first trait group to come off reth.
//!
//! A `StateProvider` is what answers `eth_getBalance`, what the txpool checks a
//! sender against, and what reth's block executor reads through. Three of its
//! seventeen methods are account-shaped, and those three now read GolemDB:
//!
//! | method | answered by |
//! | --- | --- |
//! | `basic_account` | the store's `$balance` and `$nonce` cells |
//! | `storage` | always `None` — Arkiv runs no EVM |
//! | `bytecode_by_hash` | always `None`, for the same reason |
//!
//! The other fourteen are trie-shaped or about block hashes, and still
//! delegate. They follow in their own PRs (#145).
//!
//! # Which commit a provider reads at
//!
//! reth asks for state at a block; the store numbers commits. The two are
//! bridged by the commit's host tag, which is the block hash — so a provider
//! for a block hash resolves it with
//! [`commit_by_tag`](arkiv_interfaces::store::Store::commit_by_tag), and a
//! provider for a block *number* asks reth for that number's hash first.
//!
//! Deliberately no arithmetic. Commit numbers are offset by one from block
//! numbers today, and that relationship holds only while one store has served
//! exactly one chain from genesis; a store restored from a snapshot or trimmed
//! by retention breaks it. Going through the tag is correct in every case.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue};
use arkiv_interfaces::store::{SegmentAddress, SegmentOrdinal, SegmentRow, Store as _, StoreError};
use arkiv_reth_statemanager::{
    AccountReadError, GolemAccounts, HostStore, StoredAccount, segments,
};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{AccountReader, BlockHashReader};
use reth_revm::db::BundleState;
use reth_storage_api::{
    BytecodeReader, HashedPostStateProvider, StateProofProvider, StateProvider, StateRootProvider,
    StorageRootProvider,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie::KeccakKeyHasher;
use reth_trie::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};
use std::sync::{Arc, OnceLock};

/// A `StateProvider` whose accounts come from GolemDB and whose trie-shaped
/// answers still come from reth's.
///
/// No `Debug`: `StateProviderBox` is a trait object that does not implement it.
pub struct ArkivStateProvider {
    store: HostStore,
    accounts: GolemAccounts<HostStore>,
    /// Whether this is the latest state, not one block's. See
    /// [`following_head`](Self::following_head).
    follows_head: bool,
    /// The block number of the `blocks` segment's first row, shared with the
    /// provider that built this so the lookup is resolved once per process.
    blocks_base: Arc<OnceLock<BlockNumber>>,
}

impl ArkivStateProvider {
    /// Wrap `inner`, answering accounts from `accounts`.
    ///
    /// The two must describe the same point in the chain: `accounts` reads at
    /// the commit whose tag is the block `inner` was opened for.
    pub const fn new(
        store: HostStore,
        accounts: GolemAccounts<HostStore>,
        blocks_base: Arc<OnceLock<BlockNumber>>,
    ) -> Self {
        Self {
            store,
            accounts,
            follows_head: false,
            blocks_base,
        }
    }

    /// Mark this as the latest state, not one block's.
    ///
    /// GolemDB can read only its newest commit. When a block is committed
    /// before an account read, the read fails. For the latest state it then
    /// runs once more, at the new commit. For one block's state it stays failed.
    pub const fn following_head(mut self) -> Self {
        self.follows_head = true;
        self
    }

    /// The account at `address`, read a second time as
    /// [`following_head`](Self::following_head) describes.
    fn stored_account(&self, address: Address) -> Result<Option<StoredAccount>, AccountReadError> {
        match self.accounts.account(address) {
            Err(AccountReadError::Store(StoreError::HandleInvalid)) if self.follows_head => {
                GolemAccounts::new(self.store.clone(), self.store.head()).account(address)
            }
            read => read,
        }
    }

    /// The digest the store committed at the point these accounts read.
    fn committed_root(&self) -> ProviderResult<B256> {
        self.accounts
            .commit_hash()
            .map(B256::from)
            .map_err(read_error)
    }
}

/// An account read that failed is a provider error, not an empty account.
///
/// `basic_account` returning `None` means "no such account", which is a load-
/// bearing answer: it decides EIP-161 reaping and whether a sender can pay.
/// Folding a store fault into it would turn an unreachable store into a chain
/// where everyone is broke.
fn read_error(error: impl core::fmt::Display) -> ProviderError {
    ProviderError::other(std::io::Error::other(format!(
        "golemdb account read: {error}"
    )))
}

impl AccountReader for ArkivStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self.stored_account(*address).map_err(read_error)?.map(
            |StoredAccount { balance, nonce }| Account {
                nonce,
                balance,
                // Arkiv runs no EVM, so no account has code. `None` here is
                // reth's "empty code hash", which is what every Arkiv account
                // has.
                bytecode_hash: None,
            },
        ))
    }
}

impl BytecodeReader for ArkivStateProvider {
    fn bytecode_by_hash(&self, _code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(None)
    }
}

impl StateProvider for ArkivStateProvider {
    fn storage(
        &self,
        _account: Address,
        _storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        Ok(None)
    }
}

/// Arkiv serves no merkle proofs.
///
/// A proof is a path through a Merkle-Patricia trie, and this node keeps no
/// trie: its state root is GolemDB's commit digest, a flat fingerprint that
/// no inclusion path leads to. Refused rather than answered from an empty
/// database, which would produce a proof against the wrong root.
fn no_proofs<T>(method: &'static str) -> ProviderResult<T> {
    Err(ProviderError::other(std::io::Error::other(format!(
        "arkiv has no trie, so {method} cannot be served; its state root is \
         GolemDB's commit digest"
    ))))
}

impl BlockHashReader for ArkivStateProvider {
    /// From the `blocks` segment, by the height's ordinal.
    ///
    /// `None` outside the segment -- a height below its first row or above
    /// its last. There is no longer a fallback to reth: this node does not
    /// write reth's database, so the only thing behind it is the genesis
    /// allocation.
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let Some(base) = self.blocks_base()? else {
            return Ok(None);
        };
        let Some(ordinal) = number.checked_sub(base) else {
            return Ok(None);
        };
        let Some(row) = self.block_row(ordinal)? else {
            return Ok(None);
        };
        let header = decode_row(&row)?;
        if header.number != number {
            return Ok(None);
        }
        Ok(Some(header.hash_slow()))
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<BlockHash>> {
        // Whatever the range holds, rather than an error on the first gap.
        (start..end)
            .map(|number| self.block_hash(number))
            .filter_map(Result::transpose)
            .collect()
    }
}

/// The state root, which on this node is GolemDB's commit digest.
///
/// # Only the empty-block case reaches here
///
/// A block that executed something carries its root out of execution: the
/// digest the branch reached is sealed by `BlockSeals` and installed by the
/// payload builder's state-root resolver, before the provider is consulted.
/// What the resolver cannot answer is a block that sealed nothing — an empty
/// one, which charges no gas — and it falls back to the provider for those.
///
/// An empty block changes no state, so the root is the one the store already
/// committed: the digest at the head commit, which the accounts view is
/// already pinned to.
///
/// The updates are empty because there is no trie to update. reth writes them
/// to its own database, which this node does not keep.
impl StateRootProvider for ArkivStateProvider {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.committed_root()
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        self.committed_root()
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((self.committed_root()?, TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((self.committed_root()?, TrieUpdates::default()))
    }
}

impl StorageRootProvider for ArkivStateProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        no_proofs("storage_root")
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        no_proofs("storage_proof")
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        no_proofs("storage_multiproof")
    }
}

impl StateProofProvider for ArkivStateProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        no_proofs("proof")
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        no_proofs("multiproof")
    }

    fn multiproof_v2(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargetsV2,
    ) -> ProviderResult<DecodedMultiProofV2> {
        no_proofs("multiproof_v2")
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        no_proofs("witness")
    }
}

impl HashedPostStateProvider for ArkivStateProvider {
    /// Hashed straight from the bundle.
    ///
    /// reth's own implementation then zeroes the storage of destroyed
    /// accounts, which it reads back out of its database. Arkiv runs no EVM,
    /// so no account has storage to zero -- `storage` above answers `None`
    /// for every slot -- and the bundle is the whole answer.
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        Ok(HashedPostState::from_bundle_state::<KeccakKeyHasher>(
            bundle_state.state(),
        ))
    }
}

impl ArkivStateProvider {
    /// A row of the `blocks` segment, by ordinal.
    fn block_row(&self, ordinal: SegmentOrdinal) -> ProviderResult<Option<SegmentRow>> {
        self.store
            .segment_get(segments::BLOCKS, SegmentAddress::Ordinal(ordinal))
            .map_err(|e| {
                ProviderError::other(std::io::Error::other(format!(
                    "golemdb segment_get(blocks, #{ordinal}): {e:?}"
                )))
            })
    }

    /// The block number of the segment's first row, resolved once.
    fn blocks_base(&self) -> ProviderResult<Option<BlockNumber>> {
        if let Some(base) = self.blocks_base.get() {
            return Ok(Some(*base));
        }
        let Some(row) = self.block_row(0)? else {
            return Ok(None);
        };
        let base = decode_row(&row)?.number;
        let _ = self.blocks_base.set(base);
        Ok(Some(base))
    }
}

/// The header a `blocks` row carries.
fn decode_row(row: &SegmentRow) -> ProviderResult<Header> {
    segments::decode_block(row)
        .map(|block| block.header)
        .map_err(|e| {
            ProviderError::other(std::io::Error::other(format!(
                "golemdb blocks row is malformed: {e}"
            )))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use arkiv_interfaces::constants::{
        MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES,
    };
    use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
    use arkiv_interfaces::store::CommitId;
    use arkiv_reth_statemanager::{GenesisAccount, seed_genesis};

    const ALICE: Address = Address::repeat_byte(0xaa);

    /// A GolemDB store with Alice holding `balance`, and the commit it is at.
    fn store_with_alice(balance: u64) -> (HostStore, CommitId) {
        let limit = |bytes: usize| u32::try_from(bytes).unwrap();
        let config = arkiv_golemdb::GolemDbConfig {
            host_schemas: vec![],
            genesis: arkiv_golemdb::Genesis::new(
                arkiv_golemdb::HashAlgorithm::Keccak256,
                arkiv_golemdb::CellLimits {
                    max_cell_name_len: limit(MAX_ATTRIBUTE_NAME_BYTES),
                    max_str_len: limit(MAX_STR_BYTES),
                    max_bytes_len: limit(MAX_BYTES_VALUE_LEN),
                },
                arkiv_golemdb::RecordLimits {
                    max_cells: MAX_RECORD_CELLS as u64,
                    max_indexed_cells: MAX_RECORD_INDEXED_CELLS as u64,
                },
                None,
                arkiv_golemdb::PricingConfig::zero(),
            ),
            segments: vec![],
        };
        let store: HostStore = Arc::new(arkiv_golemdb::GolemDbStore::open_memory(&config).unwrap());
        let alice = GenesisAccount {
            address: ALICE,
            balance: U256::from(balance),
            nonce: 0,
        };
        let commit = seed_genesis(&store, [7; 32], [alice]).unwrap().unwrap();
        (store, commit)
    }

    /// Commit one more block.
    fn commit_block(store: &HostStore) {
        let branch = store.begin(None).unwrap();
        store.commit(branch).unwrap();
    }

    fn state_at(store: &HostStore, commit: CommitId) -> ArkivStateProvider {
        let accounts = GolemAccounts::new(store.clone(), commit);
        ArkivStateProvider::new(store.clone(), accounts, Arc::default())
    }

    fn balance(state: &ArkivStateProvider) -> ProviderResult<U256> {
        Ok(state.basic_account(&ALICE)?.expect("Alice exists").balance)
    }

    #[test]
    fn the_latest_state_reads_at_the_new_head_when_a_block_lands_first() {
        let (store, tip) = store_with_alice(5);
        let latest = state_at(&store, tip).following_head();
        assert_eq!(balance(&latest).unwrap(), U256::from(5));

        commit_block(&store);
        assert_eq!(balance(&latest).unwrap(), U256::from(5));
    }

    #[test]
    fn one_blocks_state_is_refused_when_a_block_lands_first() {
        let (store, commit) = store_with_alice(5);
        let block = state_at(&store, commit);
        assert_eq!(balance(&block).unwrap(), U256::from(5));

        commit_block(&store);
        let error = balance(&block).unwrap_err().to_string();
        assert!(error.contains("HandleInvalid"), "{error}");
    }
}
