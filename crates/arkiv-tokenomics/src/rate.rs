//! An entity's `$rate`: the wei per block its storage was bought at.
//!
//! Each operation keeps what the entity's earlier purchases still cover and
//! adds its own, unrounded purchase, spread over the lifetime that remains:
//!
//! ```text
//! added_fee_µ = fee_base × capacity_rate × storage_time(op)        wei × 10⁶, exact
//! rate_kept   = floor(old_rate × min(units_after, units_before) / units_before)
//! new_rate    = floor((rate_kept × (old_expires_at − block_number) × 10⁶ + added_fee_µ)
//!                     / ((new_expires_at − block_number) × 10⁶))
//! ```
//!
//! One rule covers create (`old_rate` 0), extend, and growing and shrinking
//! patches. The purchase keeps its fractional wei until the one division:
//! dividing `fee_base × capacity_rate` by 10⁶ first would understate rates.
//!
//! A shrink keeps the share `units_after / units_before`, both measured with
//! the weights in force now; after a weight change that share is not the
//! purchase-time proportion. Deterministic, and accepted for iteration 1.

use arkiv_interfaces::primitives::BlockNumber;

use crate::{FeeSchedule, Overflow};

/// `capacity_rate` is in micro-gas per unit-block.
const MICRO: u128 = 1_000_000;

/// One operation's effect on an entity's `$rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateUpdate {
    /// The entity's `$rate` before the operation; 0 for a create.
    pub old_rate: u64,
    /// Its storage units before the operation; 0 for a create.
    pub units_before: u64,
    /// Its storage units after the operation.
    pub units_after: u64,
    /// Its expiry before the operation. Unused when there is nothing to keep.
    pub old_expires_at: BlockNumber,
    /// Its expiry after the operation.
    pub new_expires_at: BlockNumber,
    /// The block the operation runs in.
    pub block_number: BlockNumber,
    /// Wei per gas the storage is bought at: the base fee in iteration 1,
    /// `min_base_fee` in the spec.
    pub fee_base: u64,
    /// The operation's own storage time, unrounded.
    pub storage_time: u128,
}

impl RateUpdate {
    /// The entity's `$rate` after the operation.
    pub fn new_rate(&self, schedule: &FeeSchedule) -> Result<u64, Overflow> {
        let added = u128::from(self.fee_base)
            .checked_mul(u128::from(schedule.capacity_rate))
            .and_then(|price| price.checked_mul(self.storage_time))
            .ok_or(Overflow)?;
        let kept = match self.rate_kept() {
            0 => 0,
            rate => blocks(self.old_expires_at, self.block_number)?
                .checked_mul(rate)
                .and_then(|wei| wei.checked_mul(MICRO))
                .ok_or(Overflow)?,
        };
        let spread = blocks(self.new_expires_at, self.block_number)?
            .checked_mul(MICRO)
            .filter(|&spread| spread > 0)
            .ok_or(Overflow)?;
        let total = kept.checked_add(added).ok_or(Overflow)?;
        u64::try_from(total / spread).map_err(|_| Overflow)
    }

    /// The part of `old_rate` the units that remain still carry. All of it
    /// unless the entity shrank; nothing for a create.
    fn rate_kept(&self) -> u128 {
        match self.units_before {
            0 => 0,
            before => {
                let kept_units = self.units_after.min(before);
                u128::from(self.old_rate) * u128::from(kept_units) / u128::from(before)
            }
        }
    }
}

/// The blocks from `block_number` until `expires_at`.
fn blocks(expires_at: BlockNumber, block_number: BlockNumber) -> Result<u128, Overflow> {
    expires_at
        .checked_sub(block_number)
        .map(u128::from)
        .ok_or(Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GWEI: u64 = 1_000_000_000;

    fn schedule() -> FeeSchedule {
        FeeSchedule {
            capacity_rate: 14,
            cell_byte_units: 1,
            index_byte_units: 1,
            c_cell: 64,
            c_idx: 64,
            max_lifetime: 10_000_000,
        }
    }

    /// A create spreads its purchase over its lifetime: at 10 gwei and a
    /// `capacity_rate` of 14, a unit-block costs 140,000 wei.
    #[test]
    fn a_create_buys_at_the_unit_price() {
        let created = RateUpdate {
            old_rate: 0,
            units_before: 0,
            units_after: 1_000,
            old_expires_at: 0,
            new_expires_at: 1_000_000,
            block_number: 0,
            fee_base: 10 * GWEI,
            storage_time: 1_000 * 1_000_000,
        };
        assert_eq!(created.new_rate(&schedule()), Ok(140_000_000));
    }

    /// A shrink buys nothing and keeps the share of the old rate its remaining
    /// units carry, floored.
    #[test]
    fn a_shrink_keeps_its_share() {
        let shrunk = RateUpdate {
            old_rate: 140_000_001,
            units_before: 1_000,
            units_after: 300,
            old_expires_at: 1_000_000,
            new_expires_at: 1_000_000,
            block_number: 100_000,
            fee_base: 10 * GWEI,
            storage_time: 0,
        };
        assert_eq!(shrunk.new_rate(&schedule()), Ok(42_000_000));
    }

    /// The fractional wei survive until the one division. At 1 wei per gas
    /// and a `capacity_rate` of 1, a unit-block costs 10⁻⁶ wei: dividing the
    /// price first would make it 0 and the rate 0. Three million units for ten
    /// blocks are 3 wei per block.
    #[test]
    fn fractions_survive_until_the_end() {
        let bought = RateUpdate {
            old_rate: 0,
            units_before: 0,
            units_after: 3_000_000,
            old_expires_at: 0,
            new_expires_at: 10,
            block_number: 0,
            fee_base: 1,
            storage_time: 3_000_000 * 10,
        };
        let schedule = FeeSchedule {
            capacity_rate: 1,
            ..schedule()
        };
        assert_eq!(bought.new_rate(&schedule), Ok(3));
    }

    #[test]
    fn an_ended_lifetime_is_an_overflow() {
        let ended = RateUpdate {
            old_rate: 0,
            units_before: 0,
            units_after: 1,
            old_expires_at: 0,
            new_expires_at: 100,
            block_number: 100,
            fee_base: GWEI,
            storage_time: 0,
        };
        assert_eq!(ended.new_rate(&schedule()), Err(Overflow));
    }

    /// The largest base fee and the largest storage time a `u64` size and
    /// lifetime allow do not fit; the update fails, it does not wrap.
    #[test]
    fn a_rate_that_does_not_fit_is_an_overflow() {
        let huge = RateUpdate {
            old_rate: 0,
            units_before: 0,
            units_after: u64::MAX,
            old_expires_at: 0,
            new_expires_at: 1,
            block_number: 0,
            fee_base: u64::MAX,
            storage_time: u128::from(u64::MAX),
        };
        assert_eq!(huge.new_rate(&schedule()), Err(Overflow));
    }

    /// Every step fits `u128`, and only the result decides: a rate of exactly
    /// `u64::MAX` is stored, twice that is too large for the cell. One unit for
    /// one block at `u64::MAX` wei per gas and a `capacity_rate` of 10⁶ is
    /// `u64::MAX` wei per block.
    #[test]
    fn the_result_must_fit_the_rate_cell() {
        let at_the_limit = RateUpdate {
            old_rate: 0,
            units_before: 0,
            units_after: 1,
            old_expires_at: 0,
            new_expires_at: 1,
            block_number: 0,
            fee_base: u64::MAX,
            storage_time: 1,
        };
        let schedule = FeeSchedule {
            capacity_rate: 1_000_000,
            ..schedule()
        };
        assert_eq!(at_the_limit.new_rate(&schedule), Ok(u64::MAX));
        assert_eq!(
            RateUpdate {
                storage_time: 2,
                ..at_the_limit
            }
            .new_rate(&schedule),
            Err(Overflow),
            "2 × u64::MAX fits u128, not the cell"
        );
    }
}
