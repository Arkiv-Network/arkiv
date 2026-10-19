//! Writing a seed into a store's genesis commit.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256};
use arkiv_golemdb_state::accounts::{CELL_MINTED, record_key as account_key};
use arkiv_interfaces::entity_records;
use arkiv_interfaces::store::{
    BranchId, Cell, CellChange, CellName, CommitId, Store, StoreError, StoreExt, TypeId,
};
use arkiv_reth_statemanager::{GenesisAccount, write_genesis_accounts};
use eyre::{Result, bail, ensure};

use crate::SeedEntity;

/// How many keys a writer remembers, in seed order, for the manifest.
pub const SAMPLE_KEYS: usize = 16;

/// The genesis commit being built: the funded accounts, then the entities,
/// staged on one branch of an **empty** store.
pub struct GenesisWriter<'s, S: Store + ?Sized> {
    store: &'s S,
    branch: BranchId,
    funded: BTreeSet<Address>,
    minted: BTreeMap<Address, u64>,
    creators: BTreeSet<Address>,
    entities: u64,
    sample_keys: Vec<B256>,
}

/// What a sealed genesis commit holds, for the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The store's root over the whole genesis commit: the `stateHash`.
    pub state_root: B256,
    pub entities: u64,
    /// Every distinct creator, in address order.
    pub creators: Vec<Address>,
    /// Each minting creator's entity nonce after genesis.
    pub minted: BTreeMap<Address, u64>,
    /// The first [`SAMPLE_KEYS`] entity keys, in seed order.
    pub sample_keys: Vec<B256>,
    /// Records in the commit: entities, plus every account record.
    pub records: u64,
}

impl<'s, S: Store + ?Sized> GenesisWriter<'s, S> {
    /// Open the genesis branch and write `alloc` into it.
    ///
    /// The store must be empty: a genesis commit is commit 1, and a store that
    /// already has one holds a chain.
    pub fn begin(store: &'s S, alloc: Vec<GenesisAccount>) -> Result<Self> {
        let head = store.head();
        ensure!(
            head == CommitId::GENESIS,
            "the Arkiv store already holds a chain (head commit {}); seed into an empty \
             namespace",
            head.0
        );
        let branch = store
            .begin(None)
            .map_err(store_error("open the genesis branch"))?;
        let funded = alloc.iter().map(|account| account.address).collect();
        write_genesis_accounts(store, branch, alloc).map_err(store_error("write the alloc"))?;
        Ok(Self {
            store,
            branch,
            funded,
            minted: BTreeMap::new(),
            creators: BTreeSet::new(),
            entities: 0,
            sample_keys: Vec::new(),
        })
    }

    /// Stage one entity.
    pub fn add(&mut self, seed: SeedEntity) -> Result<()> {
        let key = B256::from(seed.entity.key);
        let creator = Address::from(seed.entity.creator);
        let cells = entity_records::to_cells(&seed.entity)
            .map_err(|e| eyre::eyre!("entity {key}: {e:?}"))?;
        match self.store.create(
            self.branch,
            entity_records::record_key(seed.entity.key),
            cells,
            None,
        ) {
            Ok(_) => {}
            Err(StoreError::AlreadyExists) => bail!("entity {key} is seeded twice"),
            Err(error) => bail!("write entity {key}: {error:?}"),
        }
        if seed.minted {
            *self.minted.entry(creator).or_default() += 1;
        }
        self.creators.insert(creator);
        if self.sample_keys.len() < SAMPLE_KEYS {
            self.sample_keys.push(key);
        }
        self.entities += 1;
        Ok(())
    }

    /// How many entities are staged.
    pub const fn entities(&self) -> u64 {
        self.entities
    }

    /// Advance the minting creators' nonces, then seal: the root is the
    /// store's, over everything staged. Nothing is committed yet.
    pub fn seal(self) -> Result<SealedGenesis<'s, S>> {
        for (creator, count) in &self.minted {
            let key = account_key(creator.into_array());
            let cell = Cell::field(TypeId::U64, count.to_be_bytes().to_vec());
            // As the executor's minting lane does: a funded account gains the
            // cell, anyone else gets a record holding only that cell -- which
            // is not an Ethereum account, see `GolemAccounts::account`.
            let written = if self.funded.contains(creator) {
                self.store
                    .patch(
                        self.branch,
                        key,
                        None,
                        vec![(CellName::from(CELL_MINTED), CellChange::Set(cell))],
                        None,
                    )
                    .map(drop)
            } else {
                self.store
                    .create(
                        self.branch,
                        key,
                        vec![(CellName::from(CELL_MINTED), cell)],
                        None,
                    )
                    .map(drop)
            };
            written.map_err(store_error("advance a creator's entity nonce"))?;
        }
        let sealed = self
            .store
            .seal(self.branch)
            .map_err(store_error("seal the genesis commit"))?;
        let accounts = self
            .funded
            .union(&self.minted.keys().copied().collect())
            .count() as u64;
        Ok(SealedGenesis {
            store: self.store,
            branch: self.branch,
            summary: Summary {
                state_root: B256::from(sealed.state_root),
                entities: self.entities,
                creators: self.creators.into_iter().collect(),
                minted: self.minted,
                sample_keys: self.sample_keys,
                records: self.entities + accounts,
            },
        })
    }
}

/// A genesis commit with its root known and nothing committed.
pub struct SealedGenesis<'s, S: Store + ?Sized> {
    store: &'s S,
    branch: BranchId,
    pub summary: Summary,
}

impl<S: StoreExt + ?Sized> SealedGenesis<'_, S> {
    /// Commit, tagged with the genesis hash a header naming
    /// [`state_root`](Summary::state_root) hashes to -- the tag the node looks
    /// its genesis commit up by.
    pub fn commit(self, genesis_hash: B256) -> Result<CommitId> {
        let commit = self
            .store
            .commit_tagged(self.branch, genesis_hash.0)
            .map_err(store_error("commit the genesis"))?;
        ensure!(
            commit == CommitId::CHAIN_GENESIS,
            "the genesis landed as commit {} rather than {}",
            commit.0,
            CommitId::CHAIN_GENESIS.0
        );
        Ok(commit)
    }

    /// Drop the branch: nothing reaches the store.
    pub fn discard(self) -> Result<()> {
        self.store
            .discard(self.branch)
            .map_err(store_error("discard the genesis branch"))
    }
}

fn store_error(what: &'static str) -> impl Fn(StoreError) -> eyre::Report {
    move |error| eyre::eyre!("{what}: {error:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeedSpec;
    use alloy_primitives::U256;
    use arkiv_golemdb_state::accounts::{CELL_BALANCE, CELL_NONCE};
    use arkiv_interfaces::store::ReadTarget;
    use arkiv_interfaces::store::reference::MemStore;

    fn owners() -> Vec<Address> {
        vec![Address::repeat_byte(1), Address::repeat_byte(2)]
    }

    fn written(store: &MemStore, count: u64, funded: &[Address]) -> Summary {
        let mut spec = SeedSpec::new(1337, owners());
        spec.count = count;
        spec.payload_size = 64;
        let alloc = funded
            .iter()
            .map(|address| GenesisAccount {
                address: *address,
                balance: U256::from(5),
                nonce: 0,
            })
            .collect();
        let mut writer = GenesisWriter::begin(store, alloc).unwrap();
        for i in 0..count {
            writer.add(spec.entity(i).unwrap()).unwrap();
        }
        let sealed = writer.seal().unwrap();
        let summary = sealed.summary.clone();
        sealed.commit(B256::repeat_byte(0xee)).unwrap();
        summary
    }

    #[test]
    fn the_root_is_the_committed_root_and_is_reproducible() {
        let a = MemStore::new();
        let summary = written(&a, 5, &[Address::repeat_byte(1)]);
        assert_eq!(
            a.commit_hash(CommitId::CHAIN_GENESIS).unwrap(),
            summary.state_root.0
        );
        assert_eq!(
            a.commit_by_tag([0xee; 32]).unwrap(),
            Some(CommitId::CHAIN_GENESIS)
        );
        let b = MemStore::new();
        assert_eq!(written(&b, 5, &[Address::repeat_byte(1)]), summary);
    }

    #[test]
    fn minting_creators_get_their_nonces_and_funded_ones_keep_their_balance() {
        let store = MemStore::new();
        let summary = written(&store, 5, &[Address::repeat_byte(1)]);
        assert_eq!(summary.entities, 5);
        assert_eq!(summary.minted[&Address::repeat_byte(1)], 3);
        assert_eq!(summary.minted[&Address::repeat_byte(2)], 2);
        assert_eq!(summary.records, 5 + 2);

        let read = |byte: u8| {
            store
                .get(
                    ReadTarget::Commit(CommitId::CHAIN_GENESIS),
                    account_key([byte; 20]),
                    None,
                    None,
                )
                .unwrap()
                .into_value()
                .unwrap()
        };
        let funded = read(1);
        assert!(funded.cell(CELL_BALANCE).is_some());
        assert!(funded.cell(CELL_NONCE).is_some());
        assert_eq!(funded.cell(CELL_MINTED).unwrap().value, 3u64.to_be_bytes());
        let minting_only = read(2);
        assert!(minting_only.cell(CELL_BALANCE).is_none(), "not an account");
        assert_eq!(
            minting_only.cell(CELL_MINTED).unwrap().value,
            2u64.to_be_bytes()
        );
    }

    #[test]
    fn a_key_seeded_twice_is_refused() {
        let store = MemStore::new();
        let spec = SeedSpec::new(1337, owners());
        let mut writer = GenesisWriter::begin(&store, Vec::new()).unwrap();
        writer.add(spec.entity(0).unwrap()).unwrap();
        let err = writer.add(spec.entity(0).unwrap()).unwrap_err();
        assert!(err.to_string().contains("seeded twice"), "{err}");
    }

    #[test]
    fn a_store_holding_a_chain_is_refused() {
        let store = MemStore::new();
        written(&store, 1, &[]);
        assert!(GenesisWriter::begin(&store, Vec::new()).is_err());
    }
}
