//! `commit_tags`: the adapter's own segment, one row per commit, named by the
//! commit's tag when it has one. A tag finds its row by name, and a commit by
//! the rows it published.

use arkiv_interfaces::store::StoreError;
use golemdb_api as golemdb;
use golemdb_api::{CellType, CellValue, ImmutableDataColumn, ImmutableDataRow, Width};

pub(crate) const SEGMENT: &str = "commit_tags";
const COMMIT: &str = "commit";
const TAG: &str = "tag";
const STATE_ROOT: &str = "state_root";

/// Uncompressed: a row is mostly hashes.
pub(crate) fn segment() -> golemdb::ImmutableDataSegment {
    golemdb::ImmutableDataSegment {
        name: SEGMENT.into(),
        columns: vec![
            ImmutableDataColumn::new(COMMIT).with_type(CellType::Uint(Width::W8)),
            // Bytes, so an untagged commit can leave it empty.
            ImmutableDataColumn::new(TAG),
            ImmutableDataColumn::new(STATE_ROOT).with_type(CellType::FixedBytes(Width::W32)),
        ],
        compression: golemdb::ImmutableDataCompression::None,
    }
}

/// Reads only the commit number, which is all a tag lookup needs.
pub(crate) fn commit_only() -> golemdb::ImmutableDataReadOptions {
    golemdb::ImmutableDataReadOptions::columns([COMMIT])
}

/// One commit's row: its number, its tag, and the state root it sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TagRow {
    pub(crate) commit: u64,
    pub(crate) tag: Option<[u8; 32]>,
    pub(crate) state_root: [u8; 32],
}

impl TagRow {
    pub(crate) fn to_row(self) -> ImmutableDataRow {
        ImmutableDataRow::new(vec![
            CellValue::from_u64(self.commit),
            CellValue::from_bytes(self.tag.as_ref().map_or(&[], |tag| tag.as_slice())),
            CellValue::from_bytes32(self.state_root),
        ])
    }

    /// A row that does not decode is corruption, so `Internal`.
    pub(crate) fn from_row(row: &ImmutableDataRow) -> Result<Self, StoreError> {
        let tag: Vec<u8> = column(row, TAG)?;
        Ok(Self {
            commit: commit(row)?,
            tag: match tag.as_slice() {
                [] => None,
                tag => Some(tag.try_into().map_err(|_| StoreError::Internal)?),
            },
            state_root: column(row, STATE_ROOT)?,
        })
    }
}

/// The commit number of a full row or a [`commit_only`] one.
pub(crate) fn commit(row: &ImmutableDataRow) -> Result<u64, StoreError> {
    column(row, COMMIT)
}

fn column<T: golemdb::FromImmutableDataValue>(
    row: &ImmutableDataRow,
    name: &str,
) -> Result<T, StoreError> {
    row.get(name).map_err(|_| StoreError::Internal)
}
