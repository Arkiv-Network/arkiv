//! Storage time: the unit-blocks one operation buys, and the lifetime bound a
//! create or extend must respect before it buys any.
//!
//! ```text
//! create:         units_after × (expires_at − block_number)
//! extend:         units × (new_expires_at − old_expires_at)
//! growing patch:  (units_after − units_before) × (expires_at − block_number)
//! otherwise:      0                                  (no refunds)
//! ```
//!
//! A patch pays for its net growth only: a record that grows in one cell and
//! shrinks as much in another buys nothing. Units are `u64`, so every product
//! fits `u128`; only a lifetime that has already ended is an [`Overflow`].

use arkiv_interfaces::primitives::BlockNumber;

use crate::Overflow;

/// The expiry that means "never". Iteration 1 does not sell it.
pub const PERMANENT: BlockNumber = BlockNumber::MAX;

/// A create's storage time: its whole size, until it expires.
pub fn create(
    units_after: u64,
    expires_at: BlockNumber,
    block_number: BlockNumber,
) -> Result<u128, Overflow> {
    Ok(u128::from(units_after) * remaining(expires_at, block_number)?)
}

/// An extend's storage time: the size it already has, for the blocks it adds.
/// Equal expiries add nothing.
pub fn extend(
    units: u64,
    old_expires_at: BlockNumber,
    new_expires_at: BlockNumber,
) -> Result<u128, Overflow> {
    let added = new_expires_at.checked_sub(old_expires_at).ok_or(Overflow)?;
    Ok(u128::from(units) * u128::from(added))
}

/// A patch's storage time: its net growth, until the entity expires. A patch
/// that shrinks the entity, or keeps its size, buys nothing and refunds
/// nothing.
pub fn patch(
    units_before: u64,
    units_after: u64,
    expires_at: BlockNumber,
    block_number: BlockNumber,
) -> Result<u128, Overflow> {
    let grown = units_after.saturating_sub(units_before);
    Ok(u128::from(grown) * remaining(expires_at, block_number)?)
}

/// Whether a create or extend may set `expires_at`: the remaining lifetime it
/// leaves, not the blocks it adds, must fit `max_lifetime`, the schedule's
/// [`FeeSchedule::max_lifetime`](crate::FeeSchedule::max_lifetime). Permanent
/// expiry is refused outright,
/// without relying on arithmetic to reject it.
///
/// An expiry that is not in the future is a different rule's business; this
/// one only bounds how far ahead it lies.
pub fn within_max_lifetime(
    expires_at: BlockNumber,
    block_number: BlockNumber,
    max_lifetime: BlockNumber,
) -> bool {
    expires_at != PERMANENT && expires_at.saturating_sub(block_number) <= max_lifetime
}

/// The blocks from `block_number` until `expires_at`.
fn remaining(expires_at: BlockNumber, block_number: BlockNumber) -> Result<u128, Overflow> {
    expires_at
        .checked_sub(block_number)
        .map(u128::from)
        .ok_or(Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_buys_its_size_until_expiry() {
        assert_eq!(create(1_556, 3_888_000, 0), Ok(6_049_728_000));
        assert_eq!(create(10, 150, 100), Ok(500));
    }

    #[test]
    fn an_extend_buys_only_the_added_blocks() {
        assert_eq!(extend(2_580, 3_888_000, 7_776_000), Ok(10_031_040_000));
        assert_eq!(
            extend(2_580, 3_888_000, 3_888_000),
            Ok(0),
            "no blocks added"
        );
        assert_eq!(
            extend(2_580, 3_888_000, 1),
            Err(Overflow),
            "an extend never shortens"
        );
    }

    #[test]
    fn a_patch_buys_only_its_growth() {
        assert_eq!(
            patch(1_556, 2_580, 3_888_000, 1_296_000),
            Ok(1_024 * 2_592_000)
        );
        assert_eq!(patch(2_580, 1_556, 3_888_000, 1_296_000), Ok(0), "a shrink");
        assert_eq!(
            patch(1_556, 1_556, 3_888_000, 1_296_000),
            Ok(0),
            "same size"
        );
    }

    #[test]
    fn an_ended_lifetime_is_an_overflow() {
        assert_eq!(create(10, 99, 100), Err(Overflow));
        assert_eq!(patch(1, 2, 99, 100), Err(Overflow));
    }

    /// The remaining lifetime is bounded, at exactly `max_lifetime` and one
    /// block over, and an extend whose added blocks fit is still refused when
    /// the total does not.
    #[test]
    fn the_lifetime_bound_is_on_what_remains() {
        assert!(within_max_lifetime(1_500, 500, 1_000), "exactly the bound");
        assert!(!within_max_lifetime(1_501, 500, 1_000), "one block over");
        // Expiring at 900, extended by 200 at block 100: 200 added blocks fit,
        // 1_000 remaining do too; 201 more would leave 1_001.
        assert!(within_max_lifetime(1_100, 100, 1_000));
        assert!(!within_max_lifetime(1_101, 100, 1_000));
    }

    #[test]
    fn permanent_expiry_is_refused() {
        assert!(
            !within_max_lifetime(PERMANENT, 0, BlockNumber::MAX),
            "even when the bound itself would admit it"
        );
    }
}
