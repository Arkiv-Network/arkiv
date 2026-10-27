//! The fee schedule as a store record, and the trait the executor reads it by.
//!
//! **Interim.** GolemDB's design keeps `host.*` weights in its own cost
//! schedule and returns them in the pricing snapshot each call is priced with
//! (its D9), but the pinned GolemDB does not persist or expose that schedule
//! yet. Until it does, Arkiv keeps one record shaped the same way: the same
//! names, `u64` values, a missing value makes the schedule unusable, never
//! free. Once GolemDB returns the snapshot, [`FeeScheduleSource`] reads it
//! instead and this record leaves genesis.

use alloc::vec::Vec;

use arkiv_interfaces::store::{Cell, CellKind, CellName, Record, RecordKey, TypeId};

use crate::FeeSchedule;

/// The record the schedule is kept in: `keccak256("arkiv.fee_schedule")`.
/// Account keys start with 12 zero bytes and entity keys hash other inputs, so
/// neither can be this one.
pub const RECORD_KEY: RecordKey = RecordKey([
    0x7b, 0x44, 0x05, 0x01, 0xee, 0x56, 0x13, 0x2d, 0x54, 0xca, 0x22, 0x03, 0x8e, 0xb0, 0xc5, 0x9f,
    0xb6, 0xad, 0x16, 0x89, 0xe8, 0xc2, 0xd1, 0xd1, 0xad, 0xa8, 0x88, 0xe1, 0x17, 0xc7, 0xfe, 0x5e,
]);

pub const CAPACITY_RATE: &str = "host.arkiv.capacity_rate";
pub const CELL_BYTE_UNITS: &str = "host.arkiv.cell_byte_units";
pub const INDEX_BYTE_UNITS: &str = "host.arkiv.index_byte_units";
pub const C_CELL: &str = "host.arkiv.c_cell";
pub const C_IDX: &str = "host.arkiv.c_idx";
pub const MAX_LIFETIME: &str = "host.arkiv.max_lifetime";

/// Why a stored schedule cannot be used. Every case makes pricing unavailable:
/// an operation is never priced as free for want of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleError {
    /// There is no schedule record.
    Absent,
    /// The record lacks this value.
    Missing(&'static str),
    /// This value is not a `u64` field.
    Malformed(&'static str),
}

/// Where the executor gets the schedule a block prices with: one snapshot,
/// taken at the block's base, the same for every transaction in it.
pub trait FeeScheduleSource {
    type Error;

    fn fee_schedule(&self) -> Result<FeeSchedule, Self::Error>;
}

/// The schedule's cells: one `u64` field per value.
pub fn to_cells(schedule: &FeeSchedule) -> Vec<(CellName, Cell)> {
    values(schedule)
        .into_iter()
        .map(|(name, value)| {
            (
                CellName::from(name),
                Cell::field(TypeId::U64, value.to_be_bytes().to_vec()),
            )
        })
        .collect()
}

/// The schedule a record holds, or why it holds none.
pub fn from_record(record: &Record) -> Result<FeeSchedule, ScheduleError> {
    let value = |name: &'static str| -> Result<u64, ScheduleError> {
        let cell = record.cell(name).ok_or(ScheduleError::Missing(name))?;
        if cell.kind != CellKind::Field || cell.type_id != TypeId::U64 {
            return Err(ScheduleError::Malformed(name));
        }
        cell.value
            .as_slice()
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| ScheduleError::Malformed(name))
    };
    Ok(FeeSchedule {
        capacity_rate: value(CAPACITY_RATE)?,
        cell_byte_units: value(CELL_BYTE_UNITS)?,
        index_byte_units: value(INDEX_BYTE_UNITS)?,
        c_cell: value(C_CELL)?,
        c_idx: value(C_IDX)?,
        max_lifetime: value(MAX_LIFETIME)?,
    })
}

fn values(schedule: &FeeSchedule) -> [(&'static str, u64); 6] {
    [
        (CAPACITY_RATE, schedule.capacity_rate),
        (CELL_BYTE_UNITS, schedule.cell_byte_units),
        (INDEX_BYTE_UNITS, schedule.index_byte_units),
        (C_CELL, schedule.c_cell),
        (C_IDX, schedule.c_idx),
        (MAX_LIFETIME, schedule.max_lifetime),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use arkiv_interfaces::store::RecordVersion;

    fn schedule() -> FeeSchedule {
        FeeSchedule {
            capacity_rate: 14,
            cell_byte_units: 2,
            index_byte_units: 3,
            c_cell: 64,
            c_idx: 65,
            max_lifetime: 7_884_000,
        }
    }

    fn record(cells: Vec<(CellName, Cell)>) -> Record {
        Record {
            key: RECORD_KEY,
            version: RecordVersion(1),
            cells,
            meta: None,
        }
    }

    #[test]
    fn the_key_is_the_hash_of_its_name() {
        assert_eq!(
            RECORD_KEY.0,
            alloy_primitives::keccak256(b"arkiv.fee_schedule").0
        );
    }

    #[test]
    fn a_schedule_round_trips() {
        assert_eq!(from_record(&record(to_cells(&schedule()))), Ok(schedule()));
    }

    /// Every value is a valid GolemDB cell name, within its 32-byte cap.
    #[test]
    fn every_name_fits_golemdb() {
        for (name, _) in values(&schedule()) {
            assert!(name.len() <= 32, "{name}");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
            );
        }
    }

    #[test]
    fn a_missing_value_is_refused_not_defaulted() {
        for (name, _) in values(&schedule()) {
            let cells = to_cells(&schedule())
                .into_iter()
                .filter(|(cell, _)| cell != name)
                .collect();
            assert_eq!(
                from_record(&record(cells)),
                Err(ScheduleError::Missing(name))
            );
        }
    }

    #[test]
    fn a_value_of_another_shape_is_refused() {
        let replaced = |cell: Cell| {
            let mut cells = to_cells(&schedule());
            cells[0].1 = cell;
            from_record(&record(cells))
        };
        let malformed = Err(ScheduleError::Malformed(CAPACITY_RATE));
        assert_eq!(
            replaced(Cell::field(TypeId::U64, vec![0; 7])),
            malformed,
            "too short"
        );
        assert_eq!(
            replaced(Cell::field(TypeId::U256, vec![0; 32])),
            malformed,
            "another type"
        );
        assert_eq!(
            replaced(Cell::attribute(TypeId::U64, vec![0; 8])),
            malformed,
            "an attribute"
        );
    }
}
