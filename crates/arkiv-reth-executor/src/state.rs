//! [`ExecutorState`] — the reth **write-path** bridge for the no-EVM executor.
//!
//! The custom executor doesn't run the interpreter and has no revm `Journal`:
//! [`arkiv_transact`](crate::arkiv) reads accounts through the revm
//! [`Database`] trait and *returns* an [`EvmState`] diff that reth's block
//! executor commits. `ExecutorState` is that model as a state handle — base reads
//! fall through to the `Database`, writes accumulate into an in-flight `EvmState`
//! overlay (with read-your-own-writes), and [`into_state`](ExecutorState::into_state)
//! hands the diff back for the `ResultAndState`.
//!
//! It implements the entity store's [`AccountCode`] seam, so the reth-free
//! [`CodeBackend`] logic can drive entity writes over it. (The auxiliary index's
//! storage seam is the same overlay — an `Account`'s storage slots — and joins this
//! struct later.)
//!
//! [`Database`]: reth_ethereum::evm::primitives::Database
//! [`CodeBackend`]: arkiv_reth_entitystore::CodeBackend

use alloy_primitives::{Address, Bytes, keccak256};
use arkiv_reth_entitystore::AccountCode;
use reth_ethereum::evm::{
    primitives::Database,
    revm::{
        bytecode::JumpTable,
        primitives::KECCAK_EMPTY,
        state::{Account, AccountInfo, Bytecode, EvmState},
    },
};

/// A read-through / write-accumulate view of account state for one transaction:
/// reads consult the pending diff then the base [`Database`]; writes land in the
/// diff. Consume it with [`into_state`](ExecutorState::into_state) to get the
/// `EvmState` for the transaction's `ResultAndState`.
pub struct ExecutorState<'a, DB: Database> {
    db: &'a mut DB,
    state: EvmState,
}

impl<'a, DB: Database> ExecutorState<'a, DB> {
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
        Bytecode::new_analyzed(bytes, n, table)
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

    /// Stage `info` as a touched account in the diff.
    fn stage(&mut self, addr: Address, info: AccountInfo) {
        let mut acc = Account::from(info);
        acc.mark_touch();
        self.state.insert(addr, acc);
    }
}

impl<DB: Database> AccountCode for ExecutorState<'_, DB> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::{ATTR_STRING, ATTR_UINT, Attribute, Entity};
    use reth_ethereum::evm::revm::database_interface::EmptyDB;

    fn addr() -> Address {
        Address::from([0x11; 20])
    }

    #[test]
    fn absent_account_has_empty_code() {
        let mut db = EmptyDB::default();
        let mut state = ExecutorState::new(&mut db);
        assert!(state.code(addr()).unwrap().is_empty());
    }

    #[test]
    fn arbitrary_bytes_round_trip_byte_for_byte() {
        // Bytes that would confuse the legacy analyzer: 0xFE (INVALID) marker, a
        // 0x5B (JUMPDEST) mid-stream, and a length not a multiple of 32.
        let mut db = EmptyDB::default();
        let mut state = ExecutorState::new(&mut db);
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
            content_type: b"text/plain".to_vec(),
            payload: b"hello world".to_vec(),
            attributes: vec![
                Attribute {
                    key: b"color".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"blue".to_vec(),
                },
                Attribute {
                    key: b"size".to_vec(),
                    value_type: ATTR_UINT,
                    value: vec![0u8; 32],
                },
            ],
        };
        let encoded = arkiv_reth_entitystore::encode(&entity);

        let mut db = EmptyDB::default();
        let mut state = ExecutorState::new(&mut db);
        state.set_code(addr(), encoded.clone()).unwrap();

        let read_back = state.code(addr()).unwrap();
        assert_eq!(read_back, encoded, "code must round-trip byte-for-byte");
        assert_eq!(
            arkiv_reth_entitystore::decode(&read_back).unwrap(),
            entity,
            "the record must still decode after the round-trip"
        );
    }

    #[test]
    fn clear_code_tombstones_to_empty() {
        let mut db = EmptyDB::default();
        let mut state = ExecutorState::new(&mut db);
        state.set_code(addr(), vec![0xFE, 0x00, 0x01]).unwrap();
        assert!(!state.code(addr()).unwrap().is_empty());
        state.clear_code(addr()).unwrap();
        assert!(state.code(addr()).unwrap().is_empty());
    }

    /// The staged diff is what the executor returns: touched, with the entity code.
    #[test]
    fn into_state_carries_the_touched_diff() {
        let mut db = EmptyDB::default();
        let mut state = ExecutorState::new(&mut db);
        state.set_code(addr(), vec![0xFE, 0x00, 0x01]).unwrap();
        let diff = state.into_state();
        let acc = diff.get(&addr()).expect("account staged in the diff");
        assert!(acc.is_touched());
        assert_eq!(acc.info.nonce, 1); // kept alive against EIP-161 pruning
    }
}
