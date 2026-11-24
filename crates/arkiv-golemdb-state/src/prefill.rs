//! A prefilled chain's progress, kept in the state itself.
//!
//! A prefill writes its archives into the store as one commit each, before
//! block 0 exists. To resume after a crash, and for the node to tell a
//! half-done prefill from another chain's store, the store has to say how far
//! the prefill got. It does so in one reserved record, at
//! [`PREFILL_RECORD_KEY`], with three cells:
//!
//! | cell                  | type      | holds                                                        |
//! |-----------------------|-----------|--------------------------------------------------------------|
//! | [`CELL_TOTAL`]        | `U64`     | how many archives the set has, counted before anything is written |
//! | [`CELL_COMMITS`]      | `U64`     | how many are committed so far                                |
//! | [`CELL_GENESIS_HASH`] | `BYTES32` | the genesis file's hash, without `prefilledBlockZeroStateRoot` |
//!
//! [`start`] writes the record in the alloc commit, with no archive committed.
//! [`bump`] counts one more in the same commit as each archive, so the count
//! can never disagree with the data. The record is part of the state and so of
//! the root: every store built from the same archives holds the same values.
//!
//! The key is reserved ([`is_reserved_key`]): no entity is read or written
//! there, and a user create at it reverts. The cells are `$`-prefixed fields,
//! like an account's: Arkiv's own, and never queried, so never indexed.
//!
//! [`PREFILL_RECORD_KEY`]: arkiv_interfaces::entity_records::PREFILL_RECORD_KEY
//! [`is_reserved_key`]: arkiv_interfaces::entity_records::is_reserved_key

use alloc::vec;

use arkiv_interfaces::entity_records::PREFILL_RECORD_KEY;
use arkiv_interfaces::store::{
    BranchId, Cell, CellChange, CellName, ReadTarget, Record, RecordKey, Store, StoreError, TypeId,
};

/// How many archives the prefill's set has.
pub const CELL_TOTAL: &str = "$prefillTotal";
/// How many of them are committed.
pub const CELL_COMMITS: &str = "$prefillCommits";
/// The genesis file's hash, with `prefilledBlockZeroStateRoot` left out.
pub const CELL_GENESIS_HASH: &str = "$prefillGenesisHash";

/// The prefill record's key in the store.
pub const RECORD_KEY: RecordKey = RecordKey::from_entity(PREFILL_RECORD_KEY);

/// What the prefill record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefillCounters {
    /// How many archives the set has.
    pub total: u64,
    /// How many of them are committed.
    pub commits: u64,
    /// The genesis file's hash, with `prefilledBlockZeroStateRoot` left out.
    pub genesis_hash: [u8; 32],
}

/// Why a prefill record operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefillError {
    /// The store rejected the read or write.
    Store(StoreError),
    /// A cell was missing or the wrong width to decode.
    Malformed(&'static str),
    /// [`start`] on a store that already has the record.
    AlreadyStarted,
    /// [`bump`] on a store without the record.
    NotStarted,
    /// [`bump`] with every archive already counted.
    AllCommitted,
}

impl From<StoreError> for PrefillError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// The prefill record as of `target`, or `None` on a store that was never
/// prefilled.
pub fn read<S: Store + ?Sized>(
    store: &S,
    target: ReadTarget,
) -> Result<Option<PrefillCounters>, PrefillError> {
    let Some(record) = store.get(target, RECORD_KEY, None, None)?.into_value() else {
        return Ok(None);
    };
    Ok(Some(PrefillCounters {
        total: u64::from_be_bytes(fixed(&record, CELL_TOTAL, TypeId::U64)?),
        commits: u64::from_be_bytes(fixed(&record, CELL_COMMITS, TypeId::U64)?),
        genesis_hash: fixed(&record, CELL_GENESIS_HASH, TypeId::BYTES32)?,
    }))
}

/// Write the record on `branch`: a set of `total` archives, none committed,
/// for the genesis file hashing to `genesis_hash`. Done once, in the alloc
/// commit.
pub fn start<S: Store + ?Sized>(
    store: &S,
    branch: BranchId,
    total: u64,
    genesis_hash: [u8; 32],
) -> Result<(), PrefillError> {
    let cells = vec![
        (CellName::from(CELL_TOTAL), u64_cell(total)),
        (CellName::from(CELL_COMMITS), u64_cell(0)),
        (
            CellName::from(CELL_GENESIS_HASH),
            Cell::field(TypeId::BYTES32, genesis_hash.to_vec()),
        ),
    ];
    match store.create(branch, RECORD_KEY, cells, None) {
        Ok(_) => Ok(()),
        Err(StoreError::AlreadyExists) => Err(PrefillError::AlreadyStarted),
        Err(error) => Err(error.into()),
    }
}

/// Count one more committed archive on `branch`, the branch that commits it.
/// Returns the new count; refuses to count past the total.
pub fn bump<S: Store + ?Sized>(store: &S, branch: BranchId) -> Result<u64, PrefillError> {
    let counters = read(store, ReadTarget::Branch(branch))?.ok_or(PrefillError::NotStarted)?;
    if counters.commits >= counters.total {
        return Err(PrefillError::AllCommitted);
    }
    let commits = counters.commits + 1;
    store.patch(
        branch,
        RECORD_KEY,
        None,
        vec![(
            CellName::from(CELL_COMMITS),
            CellChange::Set(u64_cell(commits)),
        )],
        None,
    )?;
    Ok(commits)
}

fn u64_cell(value: u64) -> Cell {
    Cell::field(TypeId::U64, value.to_be_bytes().to_vec())
}

/// The value of cell `name`, which must be of type `type_id` and `N` bytes
/// wide. The width alone is not enough: eight bytes of `BYTES` are not a `U64`.
fn fixed<const N: usize>(
    record: &Record,
    name: &'static str,
    type_id: TypeId,
) -> Result<[u8; N], PrefillError> {
    record
        .cell(name)
        .filter(|cell| cell.type_id == type_id)
        .and_then(|cell| cell.value.as_slice().try_into().ok())
        .ok_or(PrefillError::Malformed(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::store::reference::MemStore;

    const HASH: [u8; 32] = [0x4f; 32];

    /// The key is the hash its docs name, so no one can mistype it into
    /// another record's.
    #[test]
    fn the_key_is_the_hash_of_its_name() {
        assert_eq!(
            PREFILL_RECORD_KEY,
            alloy_primitives::keccak256("arkiv.prefill").0
        );
    }

    /// The record goes in with no archive counted, and each bump counts one,
    /// up to the total and no further.
    #[test]
    fn the_record_counts_archives_up_to_the_total() {
        let store = MemStore::new();
        assert_eq!(read(&store, ReadTarget::Commit(store.head())), Ok(None));

        let alloc = store.begin(None).unwrap();
        start(&store, alloc, 2, HASH).unwrap();
        let counters = |commits| {
            Some(PrefillCounters {
                total: 2,
                commits,
                genesis_hash: HASH,
            })
        };
        assert_eq!(read(&store, ReadTarget::Branch(alloc)), Ok(counters(0)));
        let commit = store.commit(alloc).unwrap();
        assert_eq!(read(&store, ReadTarget::Commit(commit)), Ok(counters(0)));

        for expected in 1..=2 {
            let archive = store.begin(None).unwrap();
            assert_eq!(bump(&store, archive), Ok(expected));
            let commit = store.commit(archive).unwrap();
            assert_eq!(
                read(&store, ReadTarget::Commit(commit)),
                Ok(counters(expected))
            );
        }
        let extra = store.begin(None).unwrap();
        assert_eq!(bump(&store, extra), Err(PrefillError::AllCommitted));
    }

    #[test]
    fn the_record_is_written_once() {
        let store = MemStore::new();
        let branch = store.begin(None).unwrap();
        start(&store, branch, 3, HASH).unwrap();
        assert_eq!(
            start(&store, branch, 4, HASH),
            Err(PrefillError::AlreadyStarted)
        );
    }

    #[test]
    fn a_store_never_prefilled_counts_nothing() {
        let store = MemStore::new();
        let branch = store.begin(None).unwrap();
        assert_eq!(bump(&store, branch), Err(PrefillError::NotStarted));
    }

    /// A record missing a cell, or holding one of the wrong width or type, is
    /// refused by name rather than read as a number.
    #[test]
    fn a_malformed_record_is_refused() {
        for (cells, missing) in [
            (
                vec![(CellName::from(CELL_TOTAL), u64_cell(3))],
                CELL_COMMITS,
            ),
            (
                vec![
                    (CellName::from(CELL_TOTAL), u64_cell(3)),
                    (CellName::from(CELL_COMMITS), u64_cell(0)),
                    (
                        CellName::from(CELL_GENESIS_HASH),
                        Cell::field(TypeId::BYTES, vec![1, 2, 3]),
                    ),
                ],
                CELL_GENESIS_HASH,
            ),
            // The right width, the wrong type.
            (
                vec![
                    (CellName::from(CELL_TOTAL), u64_cell(3)),
                    (
                        CellName::from(CELL_COMMITS),
                        Cell::field(TypeId::BYTES, 2u64.to_be_bytes().to_vec()),
                    ),
                    (
                        CellName::from(CELL_GENESIS_HASH),
                        Cell::field(TypeId::BYTES32, HASH.to_vec()),
                    ),
                ],
                CELL_COMMITS,
            ),
        ] {
            let store = MemStore::new();
            let branch = store.begin(None).unwrap();
            store.create(branch, RECORD_KEY, cells, None).unwrap();
            assert_eq!(
                read(&store, ReadTarget::Branch(branch)),
                Err(PrefillError::Malformed(missing))
            );
        }
    }
}
