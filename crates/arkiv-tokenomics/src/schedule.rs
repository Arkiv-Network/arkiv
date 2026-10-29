//! The values one block prices and admits with.

use alloc::{collections::BTreeMap, string::String};
use arkiv_interfaces::{primitives::BlockNumber, store::PricingModel};

/// Fixed scale for [`FeeSchedule::capacity_rate`]: one gas is
/// 1_000_000 micro-gas. This is a unit conversion, not a pricing weight.
pub const CAPACITY_RATE_SCALE: u128 = 1_000_000;

/// The `host.arkiv.*` values iteration 1 needs: five weights that price storage,
/// and the lifetime bound that admits creates and extends.
///
/// One snapshot per block, read from the active GolemDB model at the execution
/// base. Every host value must be explicit and positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSchedule {
    /// Micro-gas per storage unit per block.
    /// Divide the accumulated charge by [`CAPACITY_RATE_SCALE`],
    /// rounding up once per transaction, to obtain whole gas.
    pub capacity_rate: u64,
    /// Storage units per cell byte.
    pub cell_byte_units: u64,
    /// Storage units per index byte.
    pub index_byte_units: u64,
    /// Storage units per cell, for what a cell costs beyond its bytes.
    pub c_cell: u64,
    /// Storage units per indexed cell, likewise for an index entry.
    pub c_idx: u64,
    /// The furthest ahead, in blocks, a create or extend may set the expiry.
    pub max_lifetime: BlockNumber,
}

impl FeeSchedule {
    /// Read the host values of a validated model, without fallback prices.
    pub fn from_model(model: &PricingModel) -> Result<Self, ScheduleError> {
        if model.host_code_version != HOST_CODE_VERSION {
            return Err(ScheduleError::UnsupportedHostCode(model.host_code_version));
        }
        Self::from_weights(&model.weights)
    }

    pub fn from_weights(weights: &BTreeMap<String, u64>) -> Result<Self, ScheduleError> {
        let value = |name| {
            let value = weights
                .get(name)
                .copied()
                .ok_or(ScheduleError::Missing(name))?;
            if value == 0 {
                return Err(ScheduleError::Zero(name));
            }
            Ok(value)
        };
        Ok(Self {
            capacity_rate: value(CAPACITY_RATE)?,
            cell_byte_units: value(CELL_BYTE_UNITS)?,
            index_byte_units: value(INDEX_BYTE_UNITS)?,
            c_cell: value(C_CELL)?,
            c_idx: value(C_IDX)?,
            max_lifetime: value(MAX_LIFETIME)?,
        })
    }
}

pub const HOST_CODE_VERSION: u64 = 1;
pub const CAPACITY_RATE: &str = "host.arkiv.capacity_rate";
pub const CELL_BYTE_UNITS: &str = "host.arkiv.cell_byte_units";
pub const INDEX_BYTE_UNITS: &str = "host.arkiv.index_byte_units";
pub const C_CELL: &str = "host.arkiv.c_cell";
pub const C_IDX: &str = "host.arkiv.c_idx";
pub const MAX_LIFETIME: &str = "host.arkiv.max_lifetime";
pub const HOST_WEIGHT_NAMES: [&str; 6] = [
    CAPACITY_RATE,
    CELL_BYTE_UNITS,
    INDEX_BYTE_UNITS,
    C_CELL,
    C_IDX,
    MAX_LIFETIME,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleError {
    Missing(&'static str),
    Zero(&'static str),
    UnsupportedHostCode(u64),
}

/// One schedule from the execution base, shared by all transactions in a block.
pub trait FeeScheduleSource {
    type Error;
    fn fee_schedule(&self) -> Result<FeeSchedule, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    fn model() -> PricingModel {
        PricingModel {
            version: 7,
            golemdb_code_version: 1,
            host_code_version: HOST_CODE_VERSION,
            weights: Arc::new(
                HOST_WEIGHT_NAMES
                    .into_iter()
                    .zip([14, 2, 3, 64, 65, 7_884_000])
                    .map(|(name, value)| (String::from(name), value))
                    .collect(),
            ),
        }
    }

    #[test]
    fn converts_the_host_weights_without_confusing_the_fields() {
        assert_eq!(
            FeeSchedule::from_model(&model()),
            Ok(FeeSchedule {
                capacity_rate: 14,
                cell_byte_units: 2,
                index_byte_units: 3,
                c_cell: 64,
                c_idx: 65,
                max_lifetime: 7_884_000,
            })
        );
    }

    #[test]
    fn every_host_weight_must_be_present_and_positive() {
        for name in HOST_WEIGHT_NAMES {
            let mut model = model();
            Arc::make_mut(&mut model.weights).insert(String::from(name), 0);
            assert_eq!(
                FeeSchedule::from_model(&model),
                Err(ScheduleError::Zero(name))
            );
            Arc::make_mut(&mut model.weights).remove(name);
            assert_eq!(
                FeeSchedule::from_model(&model),
                Err(ScheduleError::Missing(name))
            );
        }
    }

    #[test]
    fn refuses_host_code_it_cannot_interpret() {
        for version in [0, HOST_CODE_VERSION + 1] {
            let mut model = model();
            model.host_code_version = version;
            assert_eq!(
                FeeSchedule::from_model(&model),
                Err(ScheduleError::UnsupportedHostCode(version))
            );
        }
    }
}
