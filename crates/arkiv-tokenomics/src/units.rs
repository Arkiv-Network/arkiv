//! Storage units: how much space a record takes, as the schedule weighs it.
//!
//! The size comes from GolemDB's `#meta`, never from a count of Arkiv's own:
//!
//! ```text
//! storage_units = cell_bytes × cell_byte_units + index_bytes × index_byte_units
//!               + cells × c_cell + indexed_cells × c_idx
//! ```
//!
//! A write's change in units weighs its [`WriteDetails`] the same way, so
//! `units_before + units_delta = units_after` exactly.

use arkiv_interfaces::store::{RecordMeta, WriteDetails};

use crate::{FeeSchedule, Overflow};

/// The storage units a record of this size takes.
pub fn storage_units(meta: &RecordMeta, schedule: &FeeSchedule) -> Result<u64, Overflow> {
    weigh(
        meta.cell_bytes,
        meta.index_bytes,
        meta.cells,
        meta.indexed_cells,
        schedule,
    )
}

/// The change in storage units a write made: what it wrote, less what it
/// deleted. Negative when the record shrank; an updated cell counts its old
/// bytes out and its new bytes in.
pub fn units_delta(details: &WriteDetails, schedule: &FeeSchedule) -> Result<i128, Overflow> {
    let grown = weigh(
        details.cell_bytes_written,
        details.index_bytes_written,
        details.cells_created,
        details.index_joins,
        schedule,
    )?;
    let shrunk = weigh(
        details.cell_bytes_deleted,
        details.index_bytes_deleted,
        details.cells_deleted,
        details.index_leaves,
        schedule,
    )?;
    Ok(i128::from(grown) - i128::from(shrunk))
}

fn weigh(
    cell_bytes: u64,
    index_bytes: u64,
    cells: u64,
    indexed_cells: u64,
    schedule: &FeeSchedule,
) -> Result<u64, Overflow> {
    [
        (cell_bytes, schedule.cell_byte_units),
        (index_bytes, schedule.index_byte_units),
        (cells, schedule.c_cell),
        (indexed_cells, schedule.c_idx),
    ]
    .into_iter()
    .try_fold(0u64, |total, (count, weight)| {
        count
            .checked_mul(weight)
            .and_then(|units| total.checked_add(units))
            .ok_or(Overflow)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule() -> FeeSchedule {
        FeeSchedule::with_placeholder_weights(1_000_000)
    }

    /// A write's change weighs each count with its own weight, on both sides.
    /// Every count and every weight differs, so a count paired with another's
    /// weight, or a written count taken for a deleted one, changes the result.
    #[test]
    fn a_writes_change_weighs_every_count_with_its_own_weight() {
        let weights = FeeSchedule {
            cell_byte_units: 2,
            index_byte_units: 3,
            c_cell: 5,
            c_idx: 7,
            ..schedule()
        };
        let details = WriteDetails {
            cells_created: 11,
            index_joins: 13,
            cell_bytes_written: 17,
            index_bytes_written: 19,
            cells_deleted: 1,
            index_leaves: 4,
            cell_bytes_deleted: 6,
            index_bytes_deleted: 9,
            ..WriteDetails::ZERO
        };
        let grown = 17 * 2 + 19 * 3 + 11 * 5 + 13 * 7;
        let shrunk = 6 * 2 + 9 * 3 + 5 + 4 * 7;
        assert_eq!(units_delta(&details, &weights), Ok(grown - shrunk));
    }

    /// Every count, each with its own weight: 60 + 31 + 3 × 64 + 2 × 64.
    #[test]
    fn units_weigh_every_count() {
        let meta = RecordMeta {
            cells: 3,
            cell_bytes: 60,
            indexed_cells: 2,
            index_bytes: 31,
        };
        assert_eq!(storage_units(&meta, &schedule()), Ok(411));

        let weights = FeeSchedule {
            cell_byte_units: 2,
            index_byte_units: 3,
            c_cell: 5,
            c_idx: 7,
            ..schedule()
        };
        assert_eq!(
            storage_units(&meta, &weights),
            Ok(60 * 2 + 31 * 3 + 3 * 5 + 2 * 7)
        );
    }

    #[test]
    fn an_absent_record_takes_nothing() {
        assert_eq!(storage_units(&RecordMeta::default(), &schedule()), Ok(0));
    }

    /// The effects GolemDB reports, as its write-details tests pin them: a
    /// create of `n` (a u64 attribute, 18 cell and 11 index bytes) and `note`
    /// (a str field, 15 cell bytes) is 33 + 11 + 2 × 64 + 64 units, the same as
    /// the record it leaves behind.
    #[test]
    fn a_create_adds_the_records_units() {
        let created = WriteDetails {
            cells_created: 2,
            index_joins: 1,
            cell_bytes_written: 33,
            index_bytes_written: 11,
            ..WriteDetails::ZERO
        };
        let after = RecordMeta {
            cells: 2,
            cell_bytes: 33,
            indexed_cells: 1,
            index_bytes: 11,
        };
        assert_eq!(units_delta(&created, &schedule()), Ok(236));
        assert_eq!(storage_units(&after, &schedule()), Ok(236));
    }

    /// GolemDB's patch: `n` changed in place, `note` removed and `extra`, as
    /// large, added. A swap of equal sizes nets nothing.
    #[test]
    fn a_patch_of_equal_sizes_nets_nothing() {
        let patched = WriteDetails {
            cells_created: 1,
            cells_updated: 1,
            cells_deleted: 1,
            index_joins: 1,
            index_leaves: 1,
            cell_bytes_written: 18 + 15,
            cell_bytes_deleted: 18 + 15,
            index_bytes_written: 11,
            index_bytes_deleted: 11,
        };
        assert_eq!(units_delta(&patched, &schedule()), Ok(0));
    }

    /// An attribute becoming a field, as GolemDB reports it for a 32-byte `v`:
    /// its 42 cell bytes are rewritten, and its 35 index bytes and its index
    /// entry go. It shrinks by 35 + 64 units.
    #[test]
    fn leaving_the_index_shrinks_the_record() {
        let demoted = WriteDetails {
            cells_updated: 1,
            index_leaves: 1,
            cell_bytes_written: 42,
            cell_bytes_deleted: 42,
            index_bytes_deleted: 35,
            ..WriteDetails::ZERO
        };
        assert_eq!(units_delta(&demoted, &schedule()), Ok(-99));
    }

    #[test]
    fn a_size_that_does_not_fit_is_an_overflow() {
        let meta = RecordMeta {
            cell_bytes: u64::MAX,
            cells: 1,
            ..RecordMeta::default()
        };
        assert_eq!(storage_units(&meta, &schedule()), Err(Overflow));
    }
}
