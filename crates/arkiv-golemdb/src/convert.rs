//! Arkiv's store types to GolemDB's and back.

use arkiv_interfaces::entity::annotations;
use arkiv_interfaces::store::{
    BranchId, Cell, CellKind, CommitId, CostUnits, Metered, Receipt, Record, RecordKey, RecordMeta,
    RecordVersion, ScheduleVersion, SegmentAddress, SegmentKey, SegmentRow, StoreError, TypeId,
    WriteDetails,
};
use golemdb_api as golemdb;
use golemdb_api::{ApiError, CellType, FloatWidth, Width};

mod query;

pub(crate) use query::{arkiv_query_result, count_op, golemdb_budget, query_op};

/// Every form of "this branch or commit is stale" becomes `HandleInvalid`: the
/// caller re-opens over the new head and executes again. Rolling back past the
/// first frame is `HandleInvalid` too, as Arkiv's `Store::rollback` specifies.
/// A full store is a machine failure, not a verdict, so it is `Internal`.
pub(crate) fn store_error(error: ApiError) -> StoreError {
    match error {
        ApiError::NotImplemented { .. } => StoreError::NotImplemented,
        // A missing cell comes only from GolemDB's `set`, which `patch` does
        // not use: its writes create a cell that is missing.
        ApiError::NotFound | ApiError::CellNotFound { .. } => StoreError::NotFound,
        ApiError::AlreadyExists => StoreError::AlreadyExists,
        ApiError::Reserved | ApiError::InvalidArgument { .. } => StoreError::InvalidArgument,
        ApiError::InvalidQuery { .. } => StoreError::InvalidQuery,
        ApiError::LimitExceeded { .. } => StoreError::LimitExceeded,
        // A verdict on the call, not a machine failure. What it spent is on the
        // receipt, which only a metered call has, and `metered_result` reads it
        // there; with no receipt, nothing is reported spent.
        ApiError::OutOfBudget { .. } => StoreError::OutOfBudget {
            spent: CostUnits::default(),
        },
        ApiError::HandleInvalid
        | ApiError::Conflict
        | ApiError::Sealed
        | ApiError::CommitUnavailable { .. }
        | ApiError::NoFrameToRollback => StoreError::HandleInvalid,
        ApiError::StoreFull | ApiError::Internal { .. } => StoreError::Internal,
        // `ApiError` is non-exhaustive, so the compiler requires this arm: an
        // error GolemDB adds later is a machine failure until it is mapped.
        _ => StoreError::Internal,
    }
}

/// A metered call's result, with GolemDB's receipt in Arkiv's terms.
///
/// Running out of budget reports what was spent by then, which GolemDB puts on
/// the receipt rather than in the error. Other failures carry no receipt:
/// Arkiv's errors have no room for one.
pub(crate) fn metered_result<T>(
    operation: &'static str,
    metered: golemdb::Metered<T>,
) -> Result<Metered<T>, StoreError> {
    let golemdb::Metered { result, receipt } = metered;
    if matches!(operation, "create" | "patch" | "delete") {
        log_write_receipt(operation, &receipt, result.as_ref().err());
    }
    match result {
        Ok(value) => Ok(Metered::new(value, arkiv_receipt(&receipt))),
        Err(ApiError::OutOfBudget { .. }) => Err(StoreError::OutOfBudget {
            spent: CostUnits(receipt.cost),
        }),
        Err(error) => Err(store_error(error)),
    }
}

/// Log writes before conversion drops the ledger or maps an error without a receipt.
/// Debug formatting is only performed when this tracing target is enabled.
fn log_write_receipt(
    operation: &'static str,
    receipt: &golemdb::Receipt,
    error: Option<&ApiError>,
) {
    tracing::trace!(
        target: "arkiv::golemdb::metering",
        operation,
        cost = receipt.cost,
        priced_at = ?receipt.priced_at,
        ledger = ?receipt.ledger,
        details = ?receipt.details,
        ?error,
        "GolemDB receipt"
    );
}

/// GolemDB's receipt as Arkiv's. GolemDB names no schedule version, so
/// `schedule` stays 0; write ledgers are logged but not carried.
pub(crate) fn arkiv_receipt(receipt: &golemdb::Receipt) -> Receipt {
    let details = &receipt.details;
    Receipt {
        cost: CostUnits(receipt.cost),
        schedule: ScheduleVersion::default(),
        priced_at: receipt.priced_at.map(commit_id),
        details: WriteDetails {
            cells_created: details.cells_created,
            cells_updated: details.cells_updated,
            cells_deleted: details.cells_deleted,
            index_joins: details.index_joins,
            index_leaves: details.index_leaves,
            cell_bytes_written: details.cell_bytes_written,
            cell_bytes_deleted: details.cell_bytes_deleted,
            index_bytes_written: details.index_bytes_written,
            index_bytes_deleted: details.index_bytes_deleted,
        },
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
        // Types Arkiv never writes.
        CellType::Int(Width::W8 | Width::W16 | Width::W32)
        | CellType::Uint(Width::W4 | Width::W16)
        | CellType::Decimal(Width::W4 | Width::W8 | Width::W16)
        | CellType::FixedBytes(Width::W4 | Width::W8 | Width::W16)
        | CellType::Float(FloatWidth::F32 | FloatWidth::F64)
        | CellType::Date32
        | CellType::Timestamp64 => return Err(StoreError::Internal),
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
/// engine and admin cells are not part of Arkiv's record, except `#meta`,
/// which becomes the record's size. A `#meta` that does not decode is a
/// corrupt store, never an unknown size.
pub(crate) fn arkiv_record(record: &golemdb::Record) -> Result<Record, StoreError> {
    let mut cells = Vec::new();
    let mut meta = None;
    for (name, value) in &record.cells {
        let name = name.as_bytes();
        if name == RecordMeta::CELL.as_bytes() {
            let decoded = golemdb::RecordMeta::decode(value).map_err(|_| StoreError::Internal)?;
            meta = Some(RecordMeta {
                cells: decoded.cells,
                cell_bytes: decoded.cell_bytes,
                indexed_cells: decoded.indexed_cells,
                index_bytes: decoded.index_bytes,
            });
            continue;
        }
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
        meta,
    })
}
