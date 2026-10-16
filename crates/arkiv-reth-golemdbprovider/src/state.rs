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
use arkiv_interfaces::store::{SegmentAddress, SegmentOrdinal, SegmentRow, Store as _};
use arkiv_reth_statemanager::{GolemAccounts, HostStore, StoredAccount, segments};
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
            blocks_base,
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
        Ok(self.accounts.account(*address).map_err(read_error)?.map(
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
