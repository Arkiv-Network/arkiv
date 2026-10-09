//! Putting the chain's genesis allocation into the store.
//!
//! Until the accounts cutover this was reth's job alone: the genesis alloc went
//! into reth's account trie, and `eth_getBalance` read it there. Now the store
//! is what answers for accounts, so the alloc has to be in the store as well —
//! otherwise every funded account starts at zero and the first transaction is
//! rejected for insufficient funds.
//!
//! # Why it is tagged with the genesis block hash
//!
//! Every other commit is tagged with the hash of the block that produced it,
//! and reads find a commit by asking for its tag. Genesis is a block like any
//! other in that respect: without the tag, `eth_getBalance(addr, "0x0")` has no
//! commit to read at.
//!
//! It also gives the chain a commit to build on. A branch is always opened over
//! head, and before this there was nothing at head on a fresh store: block 1
//! was the first commit, so the store's commit numbers started one short.

use alloy_primitives::{Address, U256};
use arkiv_golemdb_state::accounts::{CELL_BALANCE, CELL_NONCE, record_key};
use arkiv_interfaces::store::{Cell, CellName, CommitId, Store, StoreError, TypeId};

use crate::host::HostStore;

/// One account's genesis state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenesisAccount {
    /// The account's address.
    pub address: Address,
    /// Its starting balance.
    pub balance: U256,
    /// Its starting transaction nonce — almost always zero.
    pub nonce: u64,
}

/// Write `alloc` as the store's genesis commit, tagged `genesis_hash`.
///
/// Returns `None` when the store already holds a commit: a restarted node must
/// not re-apply genesis over live state, and `head() > GENESIS` is the only
/// "already applied" marker there is. That makes this safe to call on every
/// start-up, which is how it is called.
///
/// Both cells are written for every allocated account, including a zero nonce.
/// An absent cell reads as zero either way, but an account's *existence* is
/// exactly the presence of `$balance` or `$nonce`
/// ([`GolemAccounts::account`](crate::GolemAccounts::account)) — and a genesis
/// account funded with zero still exists.
pub fn seed_genesis(
    store: &HostStore,
    genesis_hash: [u8; 32],
    alloc: impl IntoIterator<Item = GenesisAccount>,
) -> Result<Option<CommitId>, StoreError> {
    if store.head() != CommitId::GENESIS {
        return Ok(None);
    }

    let branch = store.begin(None)?;
    for account in alloc {
        let cells = vec![
            (
                CellName::from(CELL_BALANCE),
                Cell::field(TypeId::U256, account.balance.to_be_bytes::<32>().to_vec()),
            ),
            (
                CellName::from(CELL_NONCE),
                Cell::field(TypeId::U64, account.nonce.to_be_bytes().to_vec()),
            ),
        ];
        // Genesis is not a metered transaction; it is the state every metered
        // transaction starts from.
        if let Err(error) = store.create(
            branch,
            record_key(account.address.into_array()),
            cells,
            None,
        ) {
            // Leave nothing half-written for the next start-up to puzzle over.
            let _ = store.discard(branch);
            return Err(error);
        }
    }
    store.commit_tagged(branch, genesis_hash).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GolemAccounts;
    use arkiv_interfaces::store::reference::MemStore;
    use std::sync::Arc;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const HASH: [u8; 32] = [7; 32];

    #[allow(clippy::arc_with_non_send_sync)] // MemStore is a RefCell; see its docs.
    fn store() -> HostStore {
        Arc::new(MemStore::new())
    }

    fn alice(balance: u64) -> Vec<GenesisAccount> {
        vec![GenesisAccount {
            address: ALICE,
            balance: U256::from(balance),
            nonce: 0,
        }]
    }

    #[test]
    fn a_funded_account_is_readable_at_the_genesis_commit() {
        let store = store();
        let commit = seed_genesis(&store, HASH, alice(1_000))
            .expect("seed")
            .expect("fresh");

        let accounts = GolemAccounts::new(store.clone(), commit);
        let account = accounts.account(ALICE).expect("read").expect("funded");
        assert_eq!(account.balance, U256::from(1_000));
        assert_eq!(account.nonce, 0);
    }

    /// The tag is what every later read goes through, so genesis has to carry
    /// one like any other block.
    #[test]
    fn the_genesis_commit_is_reachable_by_the_block_hash() {
        let store = store();
        let commit = seed_genesis(&store, HASH, alice(1))
            .expect("seed")
            .expect("fresh");
        assert_eq!(store.commit_by_tag(HASH).expect("by tag"), Some(commit));
    }

    /// A restart must not re-apply genesis over live state.
    #[test]
    fn a_store_that_already_has_a_commit_is_left_alone() {
        let store = store();
        seed_genesis(&store, HASH, alice(1_000))
            .expect("seed")
            .expect("fresh");

        assert_eq!(
            seed_genesis(&store, HASH, alice(9_999)).expect("second seed"),
            None,
            "the second call is a no-op",
        );
        let head = store.head();
        let accounts = GolemAccounts::new(store.clone(), head);
        assert_eq!(
            accounts
                .account(ALICE)
                .expect("read")
                .expect("funded")
                .balance,
            U256::from(1_000),
            "and the original balance stands",
        );
    }

    /// An account allocated zero still exists, because the cells are written.
    /// Nothing else distinguishes it from an address nobody has heard of.
    #[test]
    fn an_account_allocated_zero_still_exists() {
        let store = store();
        let commit = seed_genesis(&store, HASH, alice(0))
            .expect("seed")
            .expect("fresh");

        let accounts = GolemAccounts::new(store, commit);
        assert!(accounts.account(ALICE).expect("read").is_some());
        assert!(
            accounts
                .account(Address::repeat_byte(0xbb))
                .expect("read")
                .is_none(),
            "an address with no allocation is not an account",
        );
    }
}
