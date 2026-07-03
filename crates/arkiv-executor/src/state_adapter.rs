//! [`JournalStateAdapter`] — bridges reth/revm's [`Journal`] to
//! [`arkiv_db_engine::StateAdapter`].
//!
//! Earlier this crate hand-rolled a per-transaction write cache
//! (`CachedAccount`) over reth's read-only [`Database`], then harvested a
//! diff via `into_evm_state`. That duplicated machinery revm already
//! ships: [`Journal`] is a transaction-scoped write buffer with
//! read-your-own-writes, checkpoint/revert, and `finalize()` to produce
//! the [`EvmState`] diff.
//!
//! `JournalStateAdapter` therefore forwards every entity-engine state
//! operation straight to the live journal. `arkiv_transact` takes a
//! checkpoint before dispatch and calls `journal.finalize()` afterwards to
//! obtain the diff for the returned `ResultAndState` — no manual caching,
//! no `original_info` diff-engine workaround.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use arkiv_db_engine::StateAdapter;
use eyre::Result;
use reth_ethereum::evm::{
    primitives::Database,
    revm::{
        bytecode::JumpTable,
        context::Journal,
        context_interface::{JournalTr, journaled_state::account::JournaledAccountTr},
        primitives::KECCAK_EMPTY,
        state::Bytecode,
    },
};

// ─── JournalStateAdapter ──────────────────────────────────────────────

pub struct JournalStateAdapter<'a, DB: Database> {
    journal: &'a mut Journal<DB>,
}

impl<'a, DB: Database> JournalStateAdapter<'a, DB> {
    pub fn new(journal: &'a mut Journal<DB>) -> Self {
        Self { journal }
    }

    /// Build a `Bytecode` from raw bytes without running the EVM analyzer
    /// (same bypass as `ReadWriteStateAdapter` in op-reth: avoids
    /// O(N) `analyze_legacy` for bitmap/entity data that is never executed).
    fn make_bytecode(bytes: Bytes) -> Bytecode {
        if bytes.is_empty() {
            return Bytecode::new();
        }
        let n = bytes.len();
        let table = JumpTable::from_slice(&vec![0u8; n.div_ceil(8)], n);
        Bytecode::new_analyzed(bytes, n, table)
    }
}

// ─── StateAdapter implementation ─────────────────────────────────────

impl<DB: Database> StateAdapter for JournalStateAdapter<'_, DB> {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
        // `Journal::code` loads (and warms) the account, inserting empty
        // code if absent, so the returned bytes are always valid.
        let code = self
            .journal
            .code(*addr)
            .map_err(|e| eyre::eyre!("journal.code({addr}): {e:?}"))?;
        Ok(code.data.to_vec())
    }

    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
        let bytes: Bytes = code.into();
        let hash = if bytes.is_empty() { KECCAK_EMPTY } else { keccak256(&bytes) };
        let bytecode = Self::make_bytecode(bytes);
        let mut acc = self
            .journal
            .load_account_mut(*addr)
            .map_err(|e| eyre::eyre!("journal.load_account_mut({addr}): {e:?}"))?
            .data;
        acc.set_code(hash, bytecode);
        // EIP-161: an account with code must not be empty (nonce 0 + no
        // balance), or `finalize` prunes it. Keep it alive.
        if acc.nonce() == 0 {
            acc.set_nonce(1);
        }
        acc.touch();
        Ok(())
    }

    fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
        let mut acc = self
            .journal
            .load_account_mut(*addr)
            .map_err(|e| eyre::eyre!("journal.load_account_mut({addr}): {e:?}"))?
            .data;
        acc.set_code(KECCAK_EMPTY, Bytecode::new());
        if acc.nonce() == 0 {
            acc.set_nonce(1);
        }
        acc.touch();
        Ok(())
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        // `sload` assumes the account is already present in the journal.
        self.journal
            .load_account(*addr)
            .map_err(|e| eyre::eyre!("journal.load_account({addr}): {e:?}"))?;
        let key = U256::from_be_bytes(slot.0);
        let val = self
            .journal
            .sload(*addr, key)
            .map_err(|e| eyre::eyre!("journal.sload({addr}, {slot}): {e:?}"))?;
        Ok(B256::from(val.data.to_be_bytes::<32>()))
    }

    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()> {
        self.journal
            .load_account(*addr)
            .map_err(|e| eyre::eyre!("journal.load_account({addr}): {e:?}"))?;
        let key = U256::from_be_bytes(slot.0);
        let val = U256::from_be_bytes(value.0);
        self.journal
            .sstore(*addr, key, val)
            .map_err(|e| eyre::eyre!("journal.sstore({addr}, {slot}): {e:?}"))?;
        Ok(())
    }

    fn ensure_account_persists(&mut self, addr: &Address) -> Result<()> {
        let mut acc = self
            .journal
            .load_account_mut(*addr)
            .map_err(|e| eyre::eyre!("journal.load_account_mut({addr}): {e:?}"))?
            .data;
        if acc.nonce() == 0 {
            acc.set_nonce(1);
        }
        acc.touch();
        Ok(())
    }

    fn iter_storage_asc(&mut self, _addr: &Address, _from: B256) -> Result<Vec<(B256, B256)>> {
        eyre::bail!("iter_storage_asc is not available during transaction execution")
    }
}
