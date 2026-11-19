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

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256};
use arkiv_golemdb_state::accounts::{CELL_BALANCE, CELL_NONCE, record_key};
use arkiv_interfaces::store::{Cell, CellName, CommitId, ReadTarget, Store, StoreError, TypeId};

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

impl GenesisAccount {
    /// The accounts of a genesis file's `alloc`, in address order.
    pub fn from_alloc(
        alloc: &BTreeMap<Address, alloy_genesis::GenesisAccount>,
    ) -> impl Iterator<Item = Self> + '_ {
        alloc.iter().map(|(address, account)| Self {
            address: *address,
            balance: account.balance,
            nonce: account.nonce.unwrap_or_default(),
        })
    }
}

/// Write `alloc` as the store's genesis commit,
/// tagged `genesis_hash`.
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
/// GolemDB has already persisted its pricing model before accounts are seeded.
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

/// Get `store` ready to run the chain whose block 0 hashes to `genesis_hash`,
/// or refuse it with what to do instead.
///
/// `prefilled` is whether the genesis file sets
/// `config.arkiv.prefilledBlockZeroStateRoot`: whether this chain's block 0
/// comes from a prefill. Only a fresh store of a chain that is not prefilled is
/// written to, with `alloc` as its genesis commit (see [`seed_genesis`]); the
/// commit is returned then. A store that already runs this chain is left as it
/// is. Every other [`StoreState`] is refused: an empty store for a prefilled
/// chain, an unfinished prefill, a prefill whose block 0 is not finalized, and
/// another chain's store, such as a GolemDB directory kept across a chain
/// reset.
///
/// A prefilled store started with a genesis file that lacks the field is one
/// of the last: without it, block 0's root is rebuilt from the alloc, so block
/// 0 hashes differently and no commit carries that hash. It looks like any
/// other prefilled chain's store, so the refusal names both.
pub fn prepare_genesis(
    store: &HostStore,
    genesis_hash: [u8; 32],
    prefilled: bool,
    alloc: impl IntoIterator<Item = GenesisAccount>,
) -> eyre::Result<Option<CommitId>> {
    match classify(store, genesis_hash)? {
        StoreState::GolemDbInitialized if !prefilled => seed_genesis(store, genesis_hash, alloc)
            .map_err(|e| eyre::eyre!("seed the Arkiv store with the genesis allocation: {e:?}")),
        StoreState::GolemDbInitialized => eyre::bail!(
            "the genesis file sets config.arkiv.prefilledBlockZeroStateRoot, so this chain's \
             block 0 comes from a prefill, but the Arkiv store is empty. Run `arkiv-reth \
             prefill` into this store first"
        ),
        StoreState::ArkivPrefilling { done, total } => eyre::bail!(
            "the Arkiv store holds an unfinished prefill: {done} of {total} archives are in. Run \
             `arkiv-reth prefill` again to resume it"
        ),
        StoreState::ArkivPrefillDone => eyre::bail!(
            "the Arkiv store holds a finished prefill whose block 0 is not finalized yet. Run \
             `arkiv-reth prefill --finalize`"
        ),
        StoreState::ArkivInitialized => Ok(None),
        StoreState::Unknown { prefilled: true } if !prefilled => eyre::bail!(
            "the Arkiv store was prefilled, but the genesis file has no \
             config.arkiv.prefilledBlockZeroStateRoot, so none of the store's commits is this \
             file's block 0 {}. Use the genesis file the prefill finalized, or a new store if \
             this one belongs to another chain",
            B256::from(genesis_hash)
        ),
        StoreState::Unknown { .. } => eyre::bail!(
            "the Arkiv store holds another chain: none of its commits is this chain's genesis \
             block {}. Use a new store, such as a new GolemDB directory",
            B256::from(genesis_hash)
        ),
    }
}

/// Check that `store`'s genesis commit, the one tagged `genesis_hash`, has
/// `state_root`, the root block 0's header carries.
pub fn check_genesis_root(
    store: &HostStore,
    genesis_hash: [u8; 32],
    state_root: [u8; 32],
) -> eyre::Result<()> {
    let read = |e| eyre::eyre!("read the Arkiv store's genesis: {e:?}");
    let commit = store
        .commit_by_tag(genesis_hash)
        .map_err(read)?
        .ok_or_else(|| {
            eyre::eyre!(
                "the Arkiv store has no genesis commit for block {}",
                B256::from(genesis_hash)
            )
        })?;
    let root = store.commit_hash(commit).map_err(read)?;
    eyre::ensure!(
        root == state_root,
        "the Arkiv store's genesis has state root {}, but block 0's header has {}: the store \
         does not hold this genesis",
        B256::from(root),
        B256::from(state_root)
    );
    Ok(())
}

/// What a store holds, as far as starting this chain on it goes.
///
/// A prefill writes its archives as commits before block 0, so a store can be
/// partway through one. These states tell that apart from a finished chain and
/// from stores the chain cannot run on; [`classify`] decides which one a store
/// is in.
///
/// A prefilled store has a fixed layout: commit 1 is the alloc, commits
/// `2 ..= 1 + total` are the archives, and commit `2 + total` is block 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreState {
    /// Only commit 0, GolemDB's own settings: nothing of Arkiv's yet.
    GolemDbInitialized,
    /// A prefill has committed `done` of its `total` archives and has more to
    /// do.
    ArkivPrefilling { done: u64, total: u64 },
    /// A prefill has committed every archive, but block 0 is not finalized.
    ArkivPrefillDone,
    /// A commit is tagged with this chain's block 0.
    ArkivInitialized,
    /// A finished prefill whose block 0 commit is tagged with another hash
    /// than this chain's. Either the node was started with a genesis file other
    /// than the one the prefill finalized, or the store belongs to another
    /// prefilled chain.
    ArkivPrefilledOtherBlockZero,
    /// A prefill record that does not fit the store's commits: more archives
    /// counted than the total, or a head the count does not explain. The store
    /// is damaged.
    ArkivPrefillInconsistent { done: u64, total: u64, head: u64 },
    /// Commits beyond commit 0, none of them this chain's block 0, and no
    /// prefill record: another chain's store.
    ForeignChain,
}

/// Which [`StoreState`] `store` is in, for the chain whose block 0 hashes to
/// `genesis_hash`.
///
/// Checked top to bottom:
///
/// 1. a commit tagged `genesis_hash` → [`StoreState::ArkivInitialized`];
/// 2. only commit 0 → [`StoreState::GolemDbInitialized`];
/// 3. no prefill record → [`StoreState::ForeignChain`];
/// 4. the head at commit `1 + done`, with `done` at most the total →
///    [`StoreState::ArkivPrefilling`], or [`StoreState::ArkivPrefillDone`] once
///    every archive is in;
/// 5. every archive in and commit `2 + total` tagged →
///    [`StoreState::ArkivPrefilledOtherBlockZero`];
/// 6. anything else → [`StoreState::ArkivPrefillInconsistent`].
///
/// The genesis tag is looked up first, so a store that already runs this
/// chain is never held up by its prefill record, which no longer matters once
/// block 0 exists.
pub fn classify(store: &HostStore, genesis_hash: [u8; 32]) -> eyre::Result<StoreState> {
    let read =
        |what: &str, e: &dyn core::fmt::Debug| eyre::eyre!("read the Arkiv store's {what}: {e:?}");
    if store
        .commit_by_tag(genesis_hash)
        .map_err(|e| read("genesis", &e))?
        .is_some()
    {
        return Ok(StoreState::ArkivInitialized);
    }
    let head = store.head();
    if head == CommitId::GENESIS {
        return Ok(StoreState::GolemDbInitialized);
    }
    let Some(counters) = arkiv_golemdb_state::prefill::read(&**store, ReadTarget::Commit(head))
        .map_err(|e| read("prefill record", &e))?
    else {
        return Ok(StoreState::ForeignChain);
    };
    let (done, total) = (counters.commits, counters.total);
    if done <= total && Some(head.0) == done.checked_add(1) {
        return Ok(if done < total {
            StoreState::ArkivPrefilling { done, total }
        } else {
            StoreState::ArkivPrefillDone
        });
    }
    let block_zero = total.checked_add(2).map(CommitId);
    if done == total
        && let Some(block_zero) = block_zero
        && head >= block_zero
        && store
            .tag_of(block_zero)
            .map_err(|e| read("block 0 commit", &e))?
            .is_some()
    {
        return Ok(StoreState::ArkivPrefilledOtherBlockZero);
    }
    Ok(StoreState::ArkivPrefillInconsistent {
        done,
        total,
        head: head.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GolemAccounts;
    use arkiv_interfaces::store::reference::MemStore;
    use std::sync::Arc;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const HASH: [u8; 32] = [7; 32];

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
    fn genesis_accounts_come_from_alloc_in_address_order() {
        let funded = alloy_genesis::GenesisAccount {
            balance: U256::from(2),
            ..Default::default()
        };
        let used = alloy_genesis::GenesisAccount {
            nonce: Some(3),
            ..Default::default()
        };
        let alloc = BTreeMap::from([
            (Address::repeat_byte(2), funded),
            (Address::repeat_byte(1), used),
        ]);
        let account = |byte, balance: u64, nonce| GenesisAccount {
            address: Address::repeat_byte(byte),
            balance: U256::from(balance),
            nonce,
        };
        assert_eq!(
            GenesisAccount::from_alloc(&alloc).collect::<Vec<_>>(),
            vec![account(1, 0, 3), account(2, 2, 0)]
        );
    }

    /// Block 0 must carry the genesis commit's root; any other root is
    /// refused, as is a store without this genesis.
    #[test]
    fn the_genesis_root_must_be_block_0s() {
        let store = store();
        let commit = seed_genesis(&store, HASH, alice(5)).unwrap().unwrap();
        let root = store.commit_hash(commit).unwrap();
        check_genesis_root(&store, HASH, root).unwrap();
        let error = check_genesis_root(&store, HASH, [1; 32]).unwrap_err();
        assert!(
            error.to_string().contains("does not hold this genesis"),
            "{error}"
        );
        let error = check_genesis_root(&store, [8; 32], root).unwrap_err();
        assert!(error.to_string().contains("no genesis commit"), "{error}");
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

#[cfg(test)]
mod root_tests {
    use super::*;
    use arkiv_interfaces::primitives::UserBalance;
    use arkiv_interfaces::statemanager::{AccountBalancesStore, ReadMode};
    use arkiv_interfaces::store::reference::MemStore;
    use std::sync::Arc;

    /// The header is going to carry `SealedCommit.state_root`, so moving a
    /// balance has to move that root. If it does not, every block advertises
    /// the same root and the field says nothing about the state behind it.
    #[test]
    fn a_balance_change_moves_the_sealed_state_root() {
        let store: HostStore = Arc::new(MemStore::new());
        seed_genesis(
            &store,
            [9; 32],
            vec![GenesisAccount {
                address: Address::repeat_byte(0xaa),
                balance: U256::from(1_000),
                nonce: 0,
            }],
        )
        .expect("seed")
        .expect("fresh");

        // A block that writes nothing.
        let idle = store.begin(Some(store.head())).expect("begin");
        let idle_root = store.seal(idle).expect("seal").state_root;
        store.discard(idle).expect("discard");

        // A block that moves a balance.
        let spent = store.begin(Some(store.head())).expect("begin");
        let mut view = arkiv_golemdb_state::GolemStateView::new(
            store.clone(),
            spent,
            store.head(),
            arkiv_interfaces::statemanager::BlockRef::new(1, [9; 32]),
        );
        view.fetch_sub_balance([0xaa; 20], UserBalance::from_u64(1))
            .expect("debit");
        assert_eq!(
            view.get_balance([0xaa; 20], ReadMode::ViewWithOverlay)
                .expect("read"),
            UserBalance::from_u64(999),
            "the write landed",
        );
        drop(view);
        let spent_root = store.seal(spent).expect("seal").state_root;

        assert_ne!(
            idle_root, spent_root,
            "a moved balance has to move the sealed state root",
        );
    }
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)] // MemStore is a RefCell; see its docs.
mod classify_tests {
    use super::*;
    use arkiv_golemdb_state::prefill;
    use arkiv_interfaces::store::reference::MemStore;
    use std::sync::Arc;

    /// This chain's block 0, and another chain's.
    const OURS: [u8; 32] = [7; 32];
    const OTHER: [u8; 32] = [8; 32];

    fn memstore() -> HostStore {
        Arc::new(MemStore::new())
    }

    /// GolemDB in memory, with Arkiv's limits.
    fn golemdb() -> HostStore {
        use arkiv_interfaces::constants::{
            MAX_ATTRIBUTE_NAME_BYTES, MAX_BYTES_VALUE_LEN, MAX_STR_BYTES,
        };
        use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
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
        Arc::new(arkiv_golemdb::GolemDbStore::open_memory(&config).unwrap())
    }

    /// One commit that writes a fresh account record: what any block does.
    fn commit_something(store: &HostStore, tag: Option<[u8; 32]>) {
        let branch = store.begin(None).unwrap();
        let account = [store.head().0 as u8; 20];
        store
            .create(
                branch,
                record_key(account),
                vec![(
                    CellName::from(CELL_NONCE),
                    Cell::field(TypeId::U64, 1u64.to_be_bytes().to_vec()),
                )],
                None,
            )
            .unwrap();
        match tag {
            Some(tag) => store.commit_tagged(branch, tag).map(|_| ()),
            None => store.commit(branch).map(|_| ()),
        }
        .unwrap();
    }

    /// A prefill of `total` archives with `done` of them committed: the alloc
    /// commit, then one commit per archive.
    fn prefill(store: &HostStore, total: u64, done: u64) {
        let alloc = store.begin(None).unwrap();
        prefill::start(&**store, alloc, total, [0x4f; 32]).unwrap();
        store.commit(alloc).unwrap();
        for _ in 0..done {
            let archive = store.begin(None).unwrap();
            prefill::bump(&**store, archive).unwrap();
            store.commit(archive).unwrap();
        }
    }

    /// A named example store: how to build it, and the state it is in.
    type Case<'a> = (&'a str, &'a dyn Fn(&HostStore), StoreState);

    /// Every example store from #319, on `store`.
    fn check_every_state(store: fn() -> HostStore) {
        let state = |build: &dyn Fn(&HostStore)| {
            let store = store();
            build(&store);
            classify(&store, OURS).unwrap()
        };
        let cases: [Case; 11] = [
            ("fresh", &|_| {}, StoreState::GolemDbInitialized),
            (
                "alloc only",
                &|s| prefill(s, 3, 0),
                StoreState::ArkivPrefilling { done: 0, total: 3 },
            ),
            (
                "crashed after A1",
                &|s| prefill(s, 3, 2),
                StoreState::ArkivPrefilling { done: 2, total: 3 },
            ),
            (
                "every archive in",
                &|s| prefill(s, 3, 3),
                StoreState::ArkivPrefillDone,
            ),
            (
                "prefilled, finalized and running",
                &|s| {
                    prefill(s, 3, 3);
                    commit_something(s, Some(OURS));
                    commit_something(s, Some([0xb1; 32]));
                },
                StoreState::ArkivInitialized,
            ),
            (
                "not prefilled, running",
                &|s| {
                    commit_something(s, Some(OURS));
                    commit_something(s, Some([0xb1; 32]));
                },
                StoreState::ArkivInitialized,
            ),
            (
                "another chain",
                &|s| {
                    commit_something(s, Some(OTHER));
                    commit_something(s, Some([0xb1; 32]));
                },
                StoreState::ForeignChain,
            ),
            (
                // Block 0 tagged with a hash other than the one asked for: the
                // same store, whether it is another prefilled chain's or this
                // chain's under a genesis file without its prefilled root.
                "another prefilled chain, running",
                &|s| {
                    prefill(s, 2, 2);
                    commit_something(s, Some(OTHER));
                    commit_something(s, Some([0xb1; 32]));
                },
                StoreState::ArkivPrefilledOtherBlockZero,
            ),
            (
                "a record that counts past its total",
                &|s| {
                    let branch = s.begin(None).unwrap();
                    let count = |n: u64| Cell::field(TypeId::U64, n.to_be_bytes().to_vec());
                    s.create(
                        branch,
                        prefill::RECORD_KEY,
                        vec![
                            (CellName::from(prefill::CELL_TOTAL), count(3)),
                            (CellName::from(prefill::CELL_COMMITS), count(4)),
                            (
                                CellName::from(prefill::CELL_GENESIS_HASH),
                                Cell::field(TypeId::BYTES32, vec![0x4f; 32]),
                            ),
                        ],
                        None,
                    )
                    .unwrap();
                    s.commit(branch).unwrap();
                    for _ in 0..4 {
                        commit_something(s, None);
                    }
                },
                StoreState::ArkivPrefillInconsistent {
                    done: 4,
                    total: 3,
                    head: 5,
                },
            ),
            (
                "commits the count does not explain",
                &|s| {
                    prefill(s, 3, 1);
                    commit_something(s, None);
                    commit_something(s, None);
                },
                StoreState::ArkivPrefillInconsistent {
                    done: 1,
                    total: 3,
                    head: 4,
                },
            ),
            (
                // Once block 0 exists the record no longer matters, so even a
                // damaged one cannot stop the node.
                "running, with a damaged prefill record",
                &|s| {
                    let branch = s.begin(None).unwrap();
                    s.create(
                        branch,
                        prefill::RECORD_KEY,
                        vec![(
                            CellName::from(prefill::CELL_TOTAL),
                            Cell::field(TypeId::BYTES, vec![1]),
                        )],
                        None,
                    )
                    .unwrap();
                    s.commit_tagged(branch, OURS).unwrap();
                },
                StoreState::ArkivInitialized,
            ),
        ];
        for (name, build, expected) in cases {
            assert_eq!(state(build), expected, "{name}");
        }
    }

    #[test]
    fn every_state_on_memstore() {
        check_every_state(memstore);
    }

    #[test]
    fn every_state_on_golemdb() {
        check_every_state(golemdb);
    }

    /// What starting the node on each kind of store does, with the genesis
    /// file saying the chain is `prefilled` or not.
    #[test]
    fn the_node_starts_only_on_a_store_that_runs_this_chain() {
        let alice = || {
            [GenesisAccount {
                address: alloy_primitives::Address::repeat_byte(0xaa),
                balance: alloy_primitives::U256::from(1),
                nonce: 0,
            }]
        };
        let refused = |build: &dyn Fn(&HostStore), prefilled, expected: &str| {
            let store = memstore();
            build(&store);
            let head = store.head();
            let error = prepare_genesis(&store, OURS, prefilled, alice())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{expected}: {error}");
            assert_eq!(store.head(), head, "a refusal writes nothing");
        };
        refused(
            &|_| {},
            true,
            "Run `arkiv-reth prefill` into this store first",
        );
        refused(&|s| prefill(s, 3, 2), false, "2 of 3 archives are in");
        refused(&|s| prefill(s, 3, 2), true, "2 of 3 archives are in");
        refused(
            &|s| prefill(s, 3, 3),
            true,
            "Run `arkiv-reth prefill --finalize`",
        );
        // A store prefilled and finalized as block 0 `OTHER` (its genesis
        // file has the prefilled root), started with a file without the field:
        // that file's block 0 is `OURS`.
        let finalized_elsewhere = |s: &HostStore| {
            prefill(s, 3, 3);
            commit_something(s, Some(OTHER));
        };
        refused(
            &finalized_elsewhere,
            false,
            "the Arkiv store was prefilled, but the genesis file has no \
             config.arkiv.prefilledBlockZeroStateRoot",
        );
        // With the field, but another root, it is another chain's store.
        refused(
            &finalized_elsewhere,
            true,
            "the Arkiv store holds another chain",
        );
        refused(
            &|s| commit_something(s, Some(OTHER)),
            false,
            "the Arkiv store holds another chain",
        );

        // A fresh store of a chain without prefill gets its genesis commit, as
        // before; a restart then leaves it alone.
        let store = memstore();
        let commit = prepare_genesis(&store, OURS, false, alice()).unwrap();
        assert_eq!(store.commit_by_tag(OURS).unwrap(), commit);
        assert_eq!(
            classify(&store, OURS).unwrap(),
            StoreState::ArkivInitialized
        );
        assert_eq!(prepare_genesis(&store, OURS, false, alice()).unwrap(), None);
        assert_eq!(store.head(), commit.unwrap(), "the restart wrote nothing");

        // A finalized prefill starts under the genesis file that says so.
        let store = memstore();
        prefill(&store, 3, 3);
        commit_something(&store, Some(OURS));
        let head = store.head();
        assert_eq!(prepare_genesis(&store, OURS, true, alice()).unwrap(), None);
        assert_eq!(store.head(), head);
    }
}
