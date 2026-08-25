//! [`WriteOverlay`] — the reth **write-path** bridge, and the base a
//! [`MptStateView`](crate::MptStateView) wraps on that path.
//!
//! The no-EVM executor has no revm `Journal`: it reads accounts through the
//! revm [`Database`] trait and *returns* an [`EvmState`] diff that reth's block
//! executor commits. `WriteOverlay` is that model as a state handle — base
//! reads fall through to the `Database`, writes accumulate into an in-flight
//! `EvmState` overlay (with read-your-own-writes), and
//! [`into_state`](WriteOverlay::into_state) hands the diff back for the
//! `ResultAndState`.
//!
//! It implements **every** raw store seam over that one overlay: the entity
//! store's [`AccountCode`] (an account's code), the index's [`IndexStorage`]
//! (its storage slots), the balance store's [`BalanceAccess`] and the nonce
//! store's [`NonceAccess`] (the account's two remaining fields). Every store
//! therefore stages into the same diff — which is the point of
//! [`write_manager`](crate::write_manager): one `into_state` hands reth a
//! transaction's entities, index writes, and sender accounting together.
//!
//! [`Database`]: reth_ethereum::evm::primitives::Database

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use arkiv_reth_mpt_committed_store::{AccountCode, BalanceAccess, IndexStorage, NonceAccess};
use reth_ethereum::evm::{
    primitives::Database,
    revm::{
        bytecode::JumpTable,
        primitives::KECCAK_EMPTY,
        state::{Account, AccountInfo, Bytecode, EvmState, EvmStorageSlot, TransactionId},
    },
};

/// A read-through / write-accumulate view of account state for one transaction:
/// reads consult the pending diff then the base [`Database`]; writes land in the
/// diff. Consume it with [`into_state`](WriteOverlay::into_state) to get the
/// `EvmState` for the transaction's `ResultAndState`.
pub struct WriteOverlay<'a, DB: Database> {
    db: &'a mut DB,
    state: EvmState,
}

impl<'a, DB: Database> WriteOverlay<'a, DB> {
    /// Start an empty diff over `db`.
    pub fn new(db: &'a mut DB) -> Self {
        Self {
            db,
            state: EvmState::default(),
        }
    }

    /// The accumulated account diff, to return in a `ResultAndState`.
    pub fn into_state(self) -> EvmState {
        self.state
    }

    /// Wrap raw bytes as `Bytecode` **without** running revm's legacy analyzer.
    ///
    /// Entity records are data, never executed, so we hand revm a pre-"analyzed"
    /// bytecode with an all-zero jump table (no valid jump destinations). This both
    /// skips the O(N) analysis and — crucially — keeps the bytes **verbatim**, so an
    /// entity record round-trips byte-for-byte rather than being padded or rewritten
    /// by the analyzer.
    fn make_bytecode(bytes: Bytes) -> Bytecode {
        if bytes.is_empty() {
            return Bytecode::new();
        }
        let n = bytes.len();
        let table = JumpTable::from_slice(&vec![0u8; n.div_ceil(8)], n);
        // SAFETY: `new_analyzed` is unsafe because unpadded bytecode would let the
        // interpreter read past the allocation while decoding PUSH immediates. This
        // node never runs the interpreter over entity code: `ArkivEvm::transact_raw`
        // applies the state transition directly, and the one revm-delegating path
        // (`transact_system_call`) only touches the EIP-4788/EIP-2935 system
        // contracts, whose code comes from the chainspec, not from here. Padding is
        // also not an option — entity records must round-trip byte-for-byte.
        unsafe { Bytecode::new_analyzed(bytes, n, table) }
    }

    /// The account's current info: the pending diff if it's been touched, else the
    /// base `Database` (default if the account is absent).
    fn load_info(&mut self, addr: Address) -> Result<AccountInfo, eyre::Report> {
        if let Some(acc) = self.state.get(&addr) {
            return Ok(acc.info.clone());
        }
        Ok(self
            .db
            .basic(addr)
            .map_err(|e| eyre::eyre!("db.basic({addr}): {e:?}"))?
            .unwrap_or_default())
    }

    /// Stage `info` as a touched account in the diff, preserving any storage the
    /// account already has staged.
    fn stage(&mut self, addr: Address, info: AccountInfo) {
        match self.state.get_mut(&addr) {
            Some(acc) => {
                acc.info = info;
                acc.mark_touch();
            }
            None => {
                let mut acc = Account::from(info);
                acc.mark_touch();
                self.state.insert(addr, acc);
            }
        }
    }

    /// The account in the diff, loading it (untouched) from the base `Database`
    /// first if it isn't there yet.
    fn account_mut(&mut self, addr: Address) -> Result<&mut Account, eyre::Report> {
        if !self.state.contains_key(&addr) {
            let info = self.load_info(addr)?;
            self.state.insert(addr, Account::from(info));
        }
        Ok(self.state.get_mut(&addr).expect("just inserted"))
    }

    /// A storage slot's value: the pending diff if it's been written, else the base
    /// `Database` (zero if never written). The `U256` counterpart of the
    /// [`IndexStorage::storage`] seam.
    pub fn read_slot(&mut self, addr: Address, slot: U256) -> Result<U256, eyre::Report> {
        if let Some(s) = self.state.get(&addr).and_then(|acc| acc.storage.get(&slot)) {
            return Ok(s.present_value);
        }
        self.db
            .storage(addr, slot)
            .map_err(|e| eyre::eyre!("db.storage({addr}): {e:?}"))
    }

    /// Write a storage slot into the diff. The `U256` counterpart of the
    /// [`IndexStorage::set_storage`] seam.
    pub fn write_slot(
        &mut self,
        addr: Address,
        slot: U256,
        value: U256,
    ) -> Result<(), eyre::Report> {
        // Keep the committed original across repeated writes so the diff reverts
        // correctly; only the present value changes.
        let existing_original = self
            .state
            .get(&addr)
            .and_then(|a| a.storage.get(&slot))
            .map(|s| s.original_value);
        let original = match existing_original {
            Some(o) => o,
            None => self
                .db
                .storage(addr, slot)
                .map_err(|e| eyre::eyre!("db.storage({addr}): {e:?}"))?,
        };
        let acc = self.account_mut(addr)?;
        acc.storage.insert(
            slot,
            EvmStorageSlot::new_changed(original, value, TransactionId::ZERO),
        );
        acc.mark_touch();
        Ok(())
    }

    /// Keep `addr` alive against EIP-161 pruning (raise its nonce to ≥ 1). Used to
    /// materialise the system account on its first storage write. The counterpart of
    /// the [`IndexStorage::ensure_account_persists`] seam.
    pub fn persist_account(&mut self, addr: Address) -> Result<(), eyre::Report> {
        let acc = self.account_mut(addr)?;
        if acc.info.nonce == 0 {
            acc.info.nonce = 1;
        }
        acc.mark_touch();
        Ok(())
    }
}

impl<DB: Database> AccountCode for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn code(&mut self, addr: Address) -> Result<Vec<u8>, Self::Error> {
        let info = self.load_info(addr)?;
        // Code may be inline on the info, or only referenced by hash in the base DB.
        if let Some(code) = info.code {
            return Ok(code.original_bytes().to_vec());
        }
        if info.code_hash == KECCAK_EMPTY {
            return Ok(Vec::new());
        }
        let code = self
            .db
            .code_by_hash(info.code_hash)
            .map_err(|e| eyre::eyre!("db.code_by_hash({}): {e:?}", info.code_hash))?;
        Ok(code.original_bytes().to_vec())
    }

    fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Self::Error> {
        let bytes: Bytes = code.into();
        let mut info = self.load_info(addr)?;
        info.code_hash = if bytes.is_empty() {
            KECCAK_EMPTY
        } else {
            keccak256(&bytes)
        };
        info.code = Some(Self::make_bytecode(bytes));
        // EIP-161: an account with code must not otherwise look empty (nonce 0, no
        // balance) or it gets pruned. Keep it alive.
        if info.nonce == 0 {
            info.nonce = 1;
        }
        self.stage(addr, info);
        Ok(())
    }

    fn clear_code(&mut self, addr: Address) -> Result<(), Self::Error> {
        let mut info = self.load_info(addr)?;
        info.code = Some(Bytecode::new());
        info.code_hash = KECCAK_EMPTY;
        // A tombstoned entity account stays alive (nonce ≥ 1) so it isn't pruned.
        if info.nonce == 0 {
            info.nonce = 1;
        }
        self.stage(addr, info);
        Ok(())
    }
}

/// The storage-slot seam over the same overlay: the tier-2 range structures, the
/// index's id bookkeeping, and the minting-nonce slots all live in account
/// storage. A thin `B256`⇄`U256` adapter over the inherent
/// [`read_slot`](WriteOverlay::read_slot) / [`write_slot`](WriteOverlay::write_slot)
/// / [`persist_account`](WriteOverlay::persist_account).
impl<DB: Database> IndexStorage for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Self::Error> {
        let value = self.read_slot(addr, U256::from_be_bytes(slot.0))?;
        Ok(B256::from(value.to_be_bytes::<32>()))
    }

    fn set_storage(&mut self, addr: Address, slot: B256, value: B256) -> Result<(), Self::Error> {
        self.write_slot(
            addr,
            U256::from_be_bytes(slot.0),
            U256::from_be_bytes(value.0),
        )
    }

    fn ensure_account_persists(&mut self, addr: Address) -> Result<(), Self::Error> {
        self.persist_account(addr)
    }
}

/// The balance seam over the same overlay: sender accounting (the gas charge)
/// and plain transfers stage into the one diff alongside the entities and the
/// index. Writes stage the account touched, *without* an EIP-161 keep-alive —
/// a plain account emptied of funds must stay prunable.
impl<DB: Database> BalanceAccess for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
        Ok(self.load_info(addr)?.balance)
    }

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
        let mut info = self.load_info(addr)?;
        info.balance = balance;
        self.stage(addr, info);
        Ok(())
    }
}

/// The nonce seam, with the same staging conventions as [`BalanceAccess`]
/// above.
impl<DB: Database> NonceAccess for WriteOverlay<'_, DB> {
    type Error = eyre::Report;

    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error> {
        Ok(self.load_info(addr)?.nonce)
    }

    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error> {
        let mut info = self.load_info(addr)?;
        info.nonce = nonce;
        self.stage(addr, info);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::CreationFlags;
    use arkiv_interfaces::entity::{Attribute, AttributeValue, Entity};
    use reth_ethereum::evm::revm::database_interface::EmptyDB;

    fn addr() -> Address {
        Address::from([0x11; 20])
    }

    #[test]
    fn absent_account_has_empty_code() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        assert!(state.code(addr()).unwrap().is_empty());
    }

    #[test]
    fn arbitrary_bytes_round_trip_byte_for_byte() {
        // Bytes that would confuse the legacy analyzer: 0xFE (INVALID) marker, a
        // 0x5B (JUMPDEST) mid-stream, and a length not a multiple of 32.
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        let raw = vec![0xFE, 0x00, 0x5B, 0x01, 0x02, 0xAB, 0xCD, 0xEF, 0x5B];
        state.set_code(addr(), raw.clone()).unwrap();
        assert_eq!(state.code(addr()).unwrap(), raw);
    }

    /// The real proof: a full entity record survives set → read and still decodes.
    /// If revm padded the bytecode, `decode` would trip its trailing-bytes check.
    #[test]
    fn entity_record_round_trips_through_the_diff() {
        let entity = Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 10,
            last_modified_at_block: 20,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"hello world".to_vec(),
            attributes: vec![
                Attribute::new(b"color".to_vec(), AttributeValue::Str("blue".into())),
                Attribute::new(b"size".to_vec(), AttributeValue::u256_from_u64(1)),
            ],
        };
        let encoded = arkiv_reth_mpt_committed_store::encode(&entity);

        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        state.set_code(addr(), encoded.clone()).unwrap();

        let read_back = state.code(addr()).unwrap();
        assert_eq!(read_back, encoded, "code must round-trip byte-for-byte");
        assert_eq!(
            arkiv_reth_mpt_committed_store::decode(&read_back).unwrap(),
            entity,
            "the record must still decode after the round-trip"
        );
    }

    #[test]
    fn clear_code_tombstones_to_empty() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        state.set_code(addr(), vec![0xFE, 0x00, 0x01]).unwrap();
        assert!(!state.code(addr()).unwrap().is_empty());
        state.clear_code(addr()).unwrap();
        assert!(state.code(addr()).unwrap().is_empty());
    }

    /// The staged diff is what the executor returns: touched, with the entity code.
    #[test]
    fn into_state_carries_the_touched_diff() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        state.set_code(addr(), vec![0xFE, 0x00, 0x01]).unwrap();
        let diff = state.into_state();
        let acc = diff.get(&addr()).expect("account staged in the diff");
        assert!(acc.is_touched());
        assert_eq!(acc.info.nonce, 1); // kept alive against EIP-161 pruning
    }

    #[test]
    fn storage_reads_and_writes_through_the_overlay() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        let slot = U256::from(7);
        assert_eq!(state.read_slot(addr(), slot).unwrap(), U256::ZERO);
        state.write_slot(addr(), slot, U256::from(42)).unwrap();
        assert_eq!(state.read_slot(addr(), slot).unwrap(), U256::from(42));
    }

    /// The `IndexStorage` (`B256`) seam and the inherent (`U256`) accessor are the
    /// same overlay: a write through one is visible through the other.
    #[test]
    fn index_storage_seam_shares_the_overlay() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        let slot = B256::from(U256::from(9));
        IndexStorage::set_storage(&mut state, addr(), slot, B256::from(U256::from(123))).unwrap();
        assert_eq!(
            IndexStorage::storage(&mut state, addr(), slot).unwrap(),
            B256::from(U256::from(123)),
        );
        // Visible through the inherent U256 accessor too.
        assert_eq!(
            state.read_slot(addr(), U256::from(9)).unwrap(),
            U256::from(123)
        );
    }

    /// The balance and nonce seams stage balance and nonce into the same diff, and
    /// deliberately without an EIP-161 keep-alive — a plain account must stay
    /// prunable.
    #[test]
    fn account_seams_stage_balance_and_nonce() {
        let mut db = EmptyDB::default();
        let mut state = WriteOverlay::new(&mut db);
        let a = addr();

        assert_eq!(state.get_balance(a).unwrap(), U256::ZERO);
        assert_eq!(state.get_nonce(a).unwrap(), 0);

        state.set_balance(a, U256::from(42)).unwrap();
        state.set_nonce(a, 7).unwrap();
        // Read-your-own-writes through the overlay.
        assert_eq!(state.get_balance(a).unwrap(), U256::from(42));
        assert_eq!(state.get_nonce(a).unwrap(), 7);

        let diff = state.into_state();
        let acc = diff.get(&a).expect("account staged in the diff");
        assert!(acc.is_touched());
        assert_eq!(acc.info.balance, U256::from(42));
        assert_eq!(acc.info.nonce, 7);
    }
}
