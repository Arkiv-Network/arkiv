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

use alloy_primitives::{Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue};
use arkiv_reth_statemanager::{GolemAccounts, HostStore, StoredAccount};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{AccountReader, BlockHashReader, StateProviderBox};
use reth_revm::db::BundleState;
use reth_storage_api::{
    BytecodeReader, HashedPostStateProvider, StateProofProvider, StateProvider, StateRootProvider,
    StorageRootProvider,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};

/// A `StateProvider` whose accounts come from GolemDB and whose trie-shaped
/// answers still come from reth's.
///
/// No `Debug`: `StateProviderBox` is a trait object that does not implement it.
pub struct ArkivStateProvider {
    inner: StateProviderBox,
    accounts: GolemAccounts<HostStore>,
}

impl ArkivStateProvider {
    /// Wrap `inner`, answering accounts from `accounts`.
    ///
    /// The two must describe the same point in the chain: `accounts` reads at
    /// the commit whose tag is the block `inner` was opened for.
    pub const fn new(inner: StateProviderBox, accounts: GolemAccounts<HostStore>) -> Self {
        Self { inner, accounts }
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

// ── Still reth's ────────────────────────────────────────────────────────────
//
// Block hashes are headers, which have not moved (#145, blocked on D20). The
// trie-shaped traits follow the header carrying GolemDB's own state root.

impl BlockHashReader for ArkivStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.inner.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<BlockHash>> {
        self.inner.canonical_hashes_range(start, end)
    }
}

impl StateRootProvider for ArkivStateProvider {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.inner.state_root(hashed_state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.inner.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.inner.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.inner.state_root_from_nodes_with_updates(input)
    }
}

impl StorageRootProvider for ArkivStateProvider {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.inner.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.inner.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.inner
            .storage_multiproof(address, slots, hashed_storage)
    }
}

impl StateProofProvider for ArkivStateProvider {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.inner.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.inner.multiproof(input, targets)
    }

    fn multiproof_v2(
        &self,
        input: TrieInput,
        targets: MultiProofTargetsV2,
    ) -> ProviderResult<DecodedMultiProofV2> {
        self.inner.multiproof_v2(input, targets)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        self.inner.witness(input, target, mode)
    }
}

impl HashedPostStateProvider for ArkivStateProvider {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        self.inner.hashed_post_state(bundle_state)
    }
}
