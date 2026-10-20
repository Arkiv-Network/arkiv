//! Arkiv's segments, and commit tags, as GolemDB immutable data.
//!
//! GolemDB declares segments in its genesis, with typed columns; Arkiv's rows
//! are opaque byte arrays, so every Arkiv column is a `bytes` column. Commit
//! tags ride in one more segment of GolemDB's own, keyed by the tag, whose row
//! names the commit and the root it sealed to: one row per tagged commit, so
//! `rows_of(commit)` is `tag_of` and `commit_hash`.

use arkiv_interfaces::store::{SegmentKey, SegmentRow, StoreError};
use golemdb_api as golemdb;
use golemdb_api::{
    CellType, CellValue, ImmutableDataColumn, ImmutableDataCompression, ImmutableDataConfig,
    ImmutableDataKey, ImmutableDataRow, ImmutableDataSegment, Width,
};

/// The segment holding one row per tagged commit.
pub(crate) const COMMIT_TAGS: &str = "commit_tags";
pub(crate) const TAG_COMMIT: &str = "commit";
pub(crate) const TAG_TAG: &str = "tag";
pub(crate) const TAG_STATE_ROOT: &str = "state_root";

/// Arkiv's segments and their column counts: `arkiv-reth-statemanager`'s
/// `segments` module is the authority, and a row of another width is refused.
const ARKIV_SEGMENTS: [(&str, usize); 3] = [("blocks", 2), ("transactions", 3), ("receipts", 1)];

/// Commits per immutable file span.
const SHARD_SPAN: u64 = 100_000;

pub(crate) fn declaration() -> ImmutableDataConfig {
    let mut segments: Vec<_> = ARKIV_SEGMENTS
        .iter()
        .map(|(name, columns)| ImmutableDataSegment {
            name: (*name).into(),
            columns: (0..*columns)
                .map(|column| ImmutableDataColumn::new(format!("c{column}")))
                .collect(),
            compression: ImmutableDataCompression::Lz4,
        })
        .collect();
    segments.push(ImmutableDataSegment {
        name: COMMIT_TAGS.into(),
        columns: vec![
            ImmutableDataColumn::new(TAG_COMMIT).with_type(CellType::Uint(Width::W8)),
            ImmutableDataColumn::new(TAG_TAG).with_type(CellType::FixedBytes(Width::W32)),
            ImmutableDataColumn::new(TAG_STATE_ROOT).with_type(CellType::FixedBytes(Width::W32)),
        ],
        compression: ImmutableDataCompression::None,
    });
    ImmutableDataConfig::new(SHARD_SPAN, segments)
}

/// Whether `segment` is one of Arkiv's. The tags segment is not: a host
/// appending to it directly could forge a commit's tag.
pub(crate) fn is_arkiv(segment: &str) -> bool {
    ARKIV_SEGMENTS.iter().any(|(name, _)| *name == segment)
}

pub(crate) fn key(key: SegmentKey) -> ImmutableDataKey {
    ImmutableDataKey(key.0)
}

pub(crate) fn golemdb_row(row: SegmentRow) -> ImmutableDataRow {
    ImmutableDataRow::new(
        row.iter()
            .map(|column| CellValue::from_bytes(column))
            .collect(),
    )
}

pub(crate) fn arkiv_row(row: &ImmutableDataRow) -> SegmentRow {
    row.values()
        .iter()
        .map(|value| value.value().to_vec())
        .collect()
}

pub(crate) fn tag_row(commit: u64, tag: [u8; 32], state_root: [u8; 32]) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![
        CellValue::from_u64(commit),
        CellValue::from_bytes32(tag),
        CellValue::from_bytes32(state_root),
    ])
}

/// One column of a tags row, read back.
pub(crate) fn tag_column<T: golemdb::FromImmutableDataValue>(
    row: &ImmutableDataRow,
    column: &str,
) -> Result<T, StoreError> {
    row.get(column).map_err(|_| StoreError::Internal)
}
