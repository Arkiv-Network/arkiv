//! Arkiv's store types to GolemDB's and back.

use arkiv_interfaces::entity::annotations;
use arkiv_interfaces::store::{
    BranchId, Cell, CellKind, CommitId, Metered, Receipt, Record, RecordKey, RecordVersion,
    SegmentAddress, SegmentKey, SegmentRow, StoreError, TypeId,
};
use golemdb_api as golemdb;
use golemdb_api::{ApiError, CellType, Width};

/// GolemDB reports no cost yet, so every receipt is empty.
pub(crate) fn metered<T>(value: T) -> Metered<T> {
    Metered::new(value, Receipt::default())
}

/// Every form of "this branch or commit is stale" becomes `HandleInvalid`: the
/// caller re-opens over the new head and executes again. Rolling back past the
/// first frame is `HandleInvalid` too, as Arkiv's `Store::rollback` specifies.
/// A full store is a machine failure, not a verdict, so it is `Internal`.
pub(crate) fn store_error(error: ApiError) -> StoreError {
    match error {
        ApiError::NotImplemented { .. } => StoreError::NotImplemented,
        ApiError::NotFound => StoreError::NotFound,
        ApiError::AlreadyExists => StoreError::AlreadyExists,
        ApiError::Reserved | ApiError::InvalidArgument { .. } => StoreError::InvalidArgument,
        ApiError::InvalidQuery { .. } => StoreError::InvalidQuery,
        ApiError::HandleInvalid
        | ApiError::Conflict
        | ApiError::Sealed
        | ApiError::CommitUnavailable { .. }
        | ApiError::NoFrameToRollback => StoreError::HandleInvalid,
        _ => StoreError::Internal,
    }
}

pub(crate) fn branch_id(branch: BranchId) -> golemdb::BranchId {
    golemdb::BranchId::new(branch.0)
}

pub(crate) fn commit_id(commit: golemdb::CommitId) -> CommitId {
    CommitId(commit.get())
}

pub(crate) fn golemdb_commit(commit: CommitId) -> golemdb::CommitId {
    golemdb::CommitId::new(commit.0)
}

pub(crate) fn record_key(key: RecordKey) -> golemdb::RecordKey {
    golemdb::RecordKey(key.0)
}

pub(crate) fn immutable_data_key(key: SegmentKey) -> golemdb::ImmutableDataKey {
    golemdb::ImmutableDataKey(key.0)
}

pub(crate) fn immutable_data_row(row: &SegmentRow) -> golemdb::ImmutableDataRow {
    golemdb::ImmutableDataRow::new(
        row.iter()
            .map(|column| golemdb::CellValue::from_bytes(column))
            .collect(),
    )
}

pub(crate) fn segment_row(row: &golemdb::ImmutableDataRow) -> SegmentRow {
    row.payloads().map(<[u8]>::to_vec).collect()
}

pub(crate) fn immutable_data_address(address: SegmentAddress) -> golemdb::ImmutableDataAddress {
    match address {
        SegmentAddress::Ordinal(ordinal) => golemdb::ImmutableDataAddress::Ordinal(ordinal),
        SegmentAddress::Key(key) => golemdb::ImmutableDataAddress::Key(immutable_data_key(key)),
    }
}

/// GolemDB has no key type, so `KEY` is stored as 32 fixed bytes.
fn cell_type(type_id: TypeId) -> Result<CellType, StoreError> {
    Ok(match type_id {
        TypeId::BOOL => CellType::Bool,
        TypeId::I32 => CellType::Int(Width::W4),
        TypeId::U256 => CellType::Uint(Width::W32),
        TypeId::DEC => CellType::Decimal(Width::W32),
        TypeId::BYTES32 | TypeId::KEY => CellType::FixedBytes(Width::W32),
        TypeId::BYTES => CellType::Bytes,
        TypeId::STR => CellType::Str,
        TypeId::U64 => CellType::Uint(Width::W8),
        TypeId::ADDR => CellType::Bytes20,
        _ => return Err(StoreError::InvalidArgument),
    })
}

/// The inverse of [`cell_type`]. A 32-byte cell reads back as `KEY` only for
/// the entity's own `$key`.
fn type_id(name: &[u8], cell_type: CellType) -> Result<TypeId, StoreError> {
    Ok(match cell_type {
        CellType::Bool => TypeId::BOOL,
        CellType::Int(Width::W4) => TypeId::I32,
        CellType::Uint(Width::W32) => TypeId::U256,
        CellType::Decimal(Width::W32) => TypeId::DEC,
        CellType::FixedBytes(Width::W32) if name == annotations::KEY => TypeId::KEY,
        CellType::FixedBytes(Width::W32) => TypeId::BYTES32,
        CellType::Bytes => TypeId::BYTES,
        CellType::Str => TypeId::STR,
        CellType::Uint(Width::W8) => TypeId::U64,
        CellType::Bytes20 => TypeId::ADDR,
        _ => return Err(StoreError::Internal),
    })
}

/// GolemDB stores signed values with the sign bit flipped, so that byte order
/// is numeric order; Arkiv stores plain two's complement.
fn flips_sign(cell_type: CellType) -> bool {
    matches!(cell_type, CellType::Int(_) | CellType::Decimal(_))
}

pub(crate) fn golemdb_cell(cell: &Cell) -> Result<golemdb::CellValue, StoreError> {
    let cell_type = cell_type(cell.type_id)?;
    let mut encoded = vec![cell_type.id()];
    encoded.extend_from_slice(&cell.value);
    if flips_sign(cell_type) && encoded.len() > 1 {
        encoded[1] ^= 0x80;
    }
    let kind = match cell.kind {
        CellKind::Attribute => golemdb::CellKind::Attribute,
        CellKind::Field => golemdb::CellKind::Field,
    };
    golemdb::CellValue::parse(encoded)
        .and_then(|value| value.with_kind(kind))
        .map_err(|_| StoreError::InvalidArgument)
}

/// The first byte of a cell name GolemDB keeps for itself, such as `#key`.
const ENGINE_NAME_PREFIX: u8 = b'#';
/// The first byte of a cell name on GolemDB's admin records.
const ADMIN_NAME_PREFIX: u8 = b'@';

/// GolemDB has no record versions, so every record is version 0. Its own
/// engine and admin cells are not part of Arkiv's record.
pub(crate) fn arkiv_record(record: &golemdb::Record) -> Result<Record, StoreError> {
    let mut cells = Vec::new();
    for (name, value) in &record.cells {
        let name = name.as_bytes();
        if matches!(
            name.first(),
            Some(&(ENGINE_NAME_PREFIX | ADMIN_NAME_PREFIX))
        ) {
            continue;
        }
        let mut bytes = value.value().to_vec();
        if flips_sign(value.cell_type()) && !bytes.is_empty() {
            bytes[0] ^= 0x80;
        }
        let kind = match value.kind() {
            golemdb::CellKind::Attribute => CellKind::Attribute,
            golemdb::CellKind::Field => CellKind::Field,
        };
        let cell = Cell {
            kind,
            type_id: type_id(name, value.cell_type())?,
            value: bytes,
        };
        let name = String::from_utf8(name.to_vec()).map_err(|_| StoreError::Internal)?;
        cells.push((name, cell));
    }
    Ok(Record {
        key: RecordKey(record.key.0),
        version: RecordVersion(0),
        cells,
    })
}
