//! One transaction's gas, metered as its operations run.
//!
//! ```text
//! compute_gas = max(tx_gas + Σ arkiv_op_cost + Σ golemdb_cost, floor_gas)
//! storage_gas = ceil(Σ storage_time × capacity_rate / 1_000_000)
//! purge_gas   = 0                                                (iteration 1)
//! ```
//!
//! The meter sums exact values and applies the floor and the rounding once,
//! when it finishes: rounding per operation would charge a batch more than the
//! same storage bought at once.

use arkiv_interfaces::gas::GasBreakdown;
use arkiv_interfaces::primitives::Gas;
use arkiv_interfaces::store::Receipt;

use crate::{FeeSchedule, Overflow, schedule::CAPACITY_RATE_SCALE};

/// The fee formula's inputs, summed over one transaction's batch. Every
/// addition is checked: a meter that cannot hold a sum fails the transaction.
///
/// Costs are added as they arrive and never taken back, so work a discarded
/// frame did stays metered. The meter is the executor's, outside the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GasMeter {
    arkiv_op_cost: Gas,
    golemdb_cost: Gas,
    storage_time: u128,
}

impl GasMeter {
    /// An empty meter, for a transaction about to run.
    pub const fn new() -> Self {
        Self {
            arkiv_op_cost: 0,
            golemdb_cost: 0,
            storage_time: 0,
        }
    }

    /// Add one operation's own cost.
    pub fn add_op_cost(&mut self, gas: Gas) -> Result<(), Overflow> {
        self.arkiv_op_cost = self.arkiv_op_cost.checked_add(gas).ok_or(Overflow)?;
        Ok(())
    }

    /// Add one store call's cost, from its receipt: one cost unit is one gas.
    pub fn add_golemdb(&mut self, receipt: &Receipt) -> Result<(), Overflow> {
        self.golemdb_cost = self
            .golemdb_cost
            .checked_add(receipt.cost.0)
            .ok_or(Overflow)?;
        Ok(())
    }

    /// Add one operation's storage time, unrounded.
    pub fn add_storage_time(&mut self, unit_blocks: u128) -> Result<(), Overflow> {
        self.storage_time = self.storage_time.checked_add(unit_blocks).ok_or(Overflow)?;
        Ok(())
    }

    /// GolemDB's costs so far.
    pub const fn golemdb_cost(&self) -> Gas {
        self.golemdb_cost
    }

    /// `tx_gas + arkiv_op_cost + golemdb_cost`, without the floor: the check
    /// before an operation writes.
    pub fn compute_so_far(&self, tx_gas: Gas) -> Result<Gas, Overflow> {
        tx_gas
            .checked_add(self.arkiv_op_cost)
            .and_then(|gas| gas.checked_add(self.golemdb_cost))
            .ok_or(Overflow)
    }

    /// The transaction's gas, with the floor applied to compute and storage
    /// rounded up, each once. It does not consume the meter, so it also serves
    /// as the running check after an operation: its total is what the
    /// transaction would pay if it ended there.
    ///
    /// The parts can each fit while their sum does not, so that is an
    /// [`Overflow`] here too: a breakdown this returns always has a total.
    pub fn finish(
        &self,
        tx_gas: Gas,
        floor_gas: Gas,
        schedule: &FeeSchedule,
    ) -> Result<GasBreakdown, Overflow> {
        let breakdown = GasBreakdown {
            compute_gas: self.compute_so_far(tx_gas)?.max(floor_gas),
            storage_gas: storage_gas(self.storage_time, schedule)?,
            purge_gas: 0,
        };
        breakdown.total().ok_or(Overflow)?;
        Ok(breakdown)
    }
}

/// `ceil(storage_time × capacity_rate / 1_000_000)`.
pub fn storage_gas(storage_time: u128, schedule: &FeeSchedule) -> Result<Gas, Overflow> {
    let micro_gas = storage_time
        .checked_mul(u128::from(schedule.capacity_rate))
        .ok_or(Overflow)?;
    Gas::try_from(micro_gas.div_ceil(CAPACITY_RATE_SCALE)).map_err(|_| Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::store::CostUnits;

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

    #[test]
    fn storage_gas_rounds_up() {
        assert_eq!(storage_gas(6_049_728_000, &schedule()), Ok(84_697));
        assert_eq!(
            storage_gas(
                1_000_000,
                &FeeSchedule {
                    capacity_rate: 1,
                    ..schedule()
                }
            ),
            Ok(1)
        );
        assert_eq!(
            storage_gas(
                1,
                &FeeSchedule {
                    capacity_rate: 1,
                    ..schedule()
                }
            ),
            Ok(1)
        );
        assert_eq!(storage_gas(0, &schedule()), Ok(0));
    }

    /// Two operations that each buy half a gas of storage pay one gas, not
    /// two: the batch is rounded once.
    #[test]
    fn storage_is_rounded_once_per_transaction() {
        let schedule = FeeSchedule {
            capacity_rate: 1,
            ..schedule()
        };
        let mut meter = GasMeter::new();
        meter.add_storage_time(500_000).unwrap();
        meter.add_storage_time(500_000).unwrap();
        assert_eq!(meter.finish(0, 0, &schedule).unwrap().storage_gas, 1);
        assert_eq!(
            storage_gas(500_000, &schedule).unwrap() * 2,
            2,
            "rounding each would have charged two"
        );
    }

    /// The floor lifts compute as a whole, once, and only compute.
    #[test]
    fn the_floor_applies_to_compute_once() {
        let mut meter = GasMeter::new();
        meter.add_op_cost(1_000).unwrap();
        meter
            .add_golemdb(&Receipt {
                cost: CostUnits(500),
                ..Receipt::default()
            })
            .unwrap();
        meter.add_storage_time(6_049_728_000).unwrap();

        assert_eq!(meter.compute_so_far(21_000), Ok(22_500));
        let floored = meter.finish(21_000, 30_000, &schedule()).unwrap();
        assert_eq!(
            floored,
            GasBreakdown {
                compute_gas: 30_000,
                storage_gas: 84_697,
                purge_gas: 0,
            }
        );
        assert_eq!(floored.total(), Some(114_697));
        assert_eq!(
            meter
                .finish(21_000, 22_000, &schedule())
                .unwrap()
                .compute_gas,
            22_500,
            "a floor below the sum changes nothing"
        );
    }

    /// Each part fits, their sum does not: `finish` refuses rather than hand
    /// back a breakdown without a total. The floor alone can do it.
    #[test]
    fn parts_that_fit_but_not_together_are_an_overflow() {
        let schedule = FeeSchedule {
            capacity_rate: 1_000_000,
            ..schedule()
        };
        let mut storage = GasMeter::new();
        storage.add_storage_time(1).unwrap();

        assert_eq!(
            storage.finish(0, Gas::MAX, &schedule),
            Err(Overflow),
            "the floor at the maximum, plus 1 gas of storage"
        );
        assert_eq!(
            storage.finish(Gas::MAX - 1, 0, &schedule).unwrap().total(),
            Some(Gas::MAX),
            "one below, the sum is exactly the maximum"
        );
        assert_eq!(
            storage.finish(Gas::MAX, 0, &schedule),
            Err(Overflow),
            "a compute sum at the maximum, plus storage"
        );
    }
}
