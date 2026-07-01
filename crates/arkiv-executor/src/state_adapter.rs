//! [`ExecutorStateAdapter`] — bridges reth's [`Database`] trait to
//! [`arkiv_db_engine::StateAdapter`].
//!
//! Unlike the revm-backed `ReadWriteStateAdapter` in `arkiv-op-reth`
//! (which writes through revm's live journal), here we have no journal:
//! `arkiv_transact` receives a `&mut DB` (read-only trie) and returns an
//! `EvmState` diff. `ExecutorStateAdapter` lazily loads accounts from
//! `DB` and accumulates all entity-engine writes in an in-memory cache.
//! Call [`ExecutorStateAdapter::into_evm_state`] after dispatch to
//! harvest the diff for merging into the returned [`ResultAndState`].

use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256, keccak256};
use arkiv_db_engine::StateAdapter;
use eyre::Result;
use reth_ethereum::evm::{
    primitives::Database,
    revm::{
        bytecode::JumpTable,
        primitives::KECCAK_EMPTY,
        state::{Account, AccountInfo, EvmState, EvmStorageSlot, Bytecode},
    },
};

// ─── Cache entry ─────────────────────────────────────────────────────

struct CachedAccount {
    info: AccountInfo,
    /// slot → (original_value_from_db, current_value)
    storage: HashMap<B256, (B256, B256)>,
    code_dirty: bool,
    storage_dirty: bool,
}

impl CachedAccount {
    fn mark_code_dirty(&mut self) {
        self.code_dirty = true;
    }
    fn mark_storage_dirty(&mut self) {
        self.storage_dirty = true;
    }
    fn is_dirty(&self) -> bool {
        self.code_dirty || self.storage_dirty
    }
}

// ─── ExecutorStateAdapter ─────────────────────────────────────────────

pub struct ExecutorStateAdapter<'a, DB: Database> {
    db: &'a mut DB,
    cache: HashMap<Address, CachedAccount>,
}

impl<'a, DB: Database> ExecutorStateAdapter<'a, DB> {
    pub fn new(db: &'a mut DB) -> Self {
        Self { db, cache: HashMap::new() }
    }

    /// Load account info from DB into cache (if not already cached).
    fn load_account(&mut self, addr: Address) -> Result<()> {
        if self.cache.contains_key(&addr) {
            return Ok(());
        }
        let info = self
            .db
            .basic(addr)
            .map_err(|e| eyre::eyre!("db.basic({addr}): {e:?}"))?
            .unwrap_or_default();
        self.cache.insert(addr, CachedAccount {
            info,
            storage: HashMap::new(),
            code_dirty: false,
            storage_dirty: false,
        });
        Ok(())
    }

    /// Consume the adapter and produce an `EvmState` from all dirty cache
    /// entries. This diff is merged with the sender/recipient accounting
    /// in `arkiv_transact` before being returned.
    pub fn into_evm_state(self) -> EvmState {
        let mut state = EvmState::default();
        for (addr, cached) in self.cache {
            if !cached.is_dirty() {
                continue;
            }
            let account = Account::from(cached.info)
                .with_storage(
                    cached.storage.into_iter().filter(|(_, (orig, cur))| orig != cur).map(
                        |(slot, (orig, cur))| {
                            (
                                U256::from_be_bytes(slot.0),
                                EvmStorageSlot::new_changed(
                                    U256::from_be_bytes(orig.0),
                                    U256::from_be_bytes(cur.0),
                                    0,
                                ),
                            )
                        },
                    ),
                )
                .with_touched_mark();
            state.insert(addr, account);
        }
        state
    }

    /// Build a `Bytecode` from raw bytes without running the EVM analyzer
    /// (same bypass as `ReadWriteStateAdapter` in op-reth: avoids
    /// O(N) `analyze_legacy` for bitmap/entity data that is never executed).
    fn make_bytecode(bytes: alloy_primitives::Bytes) -> Bytecode {
        if bytes.is_empty() {
            return Bytecode::new();
        }
        let n = bytes.len();
        let table = JumpTable::from_slice(&vec![0u8; n.div_ceil(8)], n);
        Bytecode::new_analyzed(bytes, n, table)
    }
}

// ─── StateAdapter implementation ─────────────────────────────────────

impl<DB: Database> StateAdapter for ExecutorStateAdapter<'_, DB> {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
        self.load_account(*addr)?;
        let cached = self.cache.get_mut(addr).unwrap();

        // If we already set code in this tx, return it directly.
        if cached.code_dirty {
            return Ok(cached.info.code.as_ref().map(|c: &Bytecode| c.original_bytes().to_vec()).unwrap_or_default());
        }

        // Otherwise fetch from DB via code_hash.
        let hash = cached.info.code_hash;
        if hash == KECCAK_EMPTY || hash == B256::ZERO {
            return Ok(vec![]);
        }
        // AccountInfo may already carry the code inline.
        if let Some(code) = cached.info.code.as_ref() {
            return Ok((code as &Bytecode).original_bytes().to_vec());
        }
        let bytecode = self
            .db
            .code_by_hash(hash)
            .map_err(|e| eyre::eyre!("db.code_by_hash({hash}): {e:?}"))?;
        let bytes = bytecode.original_bytes().to_vec();
        // Cache inline for subsequent reads.
        self.cache.get_mut(addr).unwrap().info.code = Some(bytecode);
        Ok(bytes)
    }

    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
        self.load_account(*addr)?;
        let cached = self.cache.get_mut(addr).unwrap();
        let bytes: alloy_primitives::Bytes = code.into();
        let hash = if bytes.is_empty() { KECCAK_EMPTY } else { keccak256(&bytes) };
        cached.info.code_hash = hash;
        cached.info.code = Some(Self::make_bytecode(bytes));
        // Ensure nonce ≥ 1 (EIP-161: account must not be empty-coded + nonce=0).
        if cached.info.nonce == 0 {
            cached.info.nonce = 1;
        }
        cached.mark_code_dirty();
        Ok(())
    }

    fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
        self.load_account(*addr)?;
        let cached = self.cache.get_mut(addr).unwrap();
        cached.info.code_hash = KECCAK_EMPTY;
        cached.info.code = Some(Bytecode::new());
        if cached.info.nonce == 0 {
            cached.info.nonce = 1;
        }
        cached.mark_code_dirty();
        Ok(())
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        self.load_account(*addr)?;
        let cached = self.cache.get_mut(addr).unwrap();

        // Return pending write if present.
        if let Some((_, cur)) = cached.storage.get(&slot) {
            return Ok(*cur);
        }

        // Load from DB.
        let slot_u256 = U256::from_be_bytes(slot.0);
        let val_u256 = self
            .db
            .storage(*addr, slot_u256)
            .map_err(|e| eyre::eyre!("db.storage({addr}, {slot}): {e:?}"))?;
        let val = B256::from(val_u256.to_be_bytes::<32>());
        // Cache as (original, current) with same value — no write yet.
        cached.storage.insert(slot, (val, val));
        Ok(val)
    }

    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()> {
        self.load_account(*addr)?;
        let cached = self.cache.get_mut(addr).unwrap();

        // Preserve original if we haven't loaded this slot yet.
        let original = if let Some((orig, _)) = cached.storage.get(&slot) {
            *orig
        } else {
            let slot_u256 = U256::from_be_bytes(slot.0);
            let orig_u256 = self
                .db
                .storage(*addr, slot_u256)
                .map_err(|e| eyre::eyre!("db.storage({addr}, {slot}): {e:?}"))?;
            B256::from(orig_u256.to_be_bytes::<32>())
        };

        cached.storage.insert(slot, (original, value));
        cached.mark_storage_dirty();
        Ok(())
    }

    fn ensure_account_persists(&mut self, addr: &Address) -> Result<()> {
        self.load_account(*addr)?;
        let cached = self.cache.get_mut(addr).unwrap();
        if cached.info.nonce == 0 {
            cached.info.nonce = 1;
            cached.mark_code_dirty();
        }
        Ok(())
    }

    fn iter_storage_asc(&mut self, _addr: &Address, _from: B256) -> Result<Vec<(B256, B256)>> {
        eyre::bail!("iter_storage_asc is not available during transaction execution")
    }
}
