//! The values one block prices and admits with.

use arkiv_interfaces::primitives::BlockNumber;

/// The `host.arkiv.*` values iteration 1 needs: five weights that price storage,
/// and the lifetime bound that admits creates and extends.
///
/// One snapshot per block, the same on every node. Until the schedule is read
/// from chain state, the weights have placeholder values; `max_lifetime` has
/// none and is always chosen explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSchedule {
    /// Micro-gas per storage unit-block: a unit-block is tiny, so a whole-gas
    /// rate would be far too coarse.
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
    /// Placeholder weights: `capacity_rate` 14, one unit per byte, 64 per cell
    /// and per indexed cell. Not calibrated.
    pub const fn with_placeholder_weights(max_lifetime: BlockNumber) -> Self {
        Self {
            capacity_rate: 14,
            cell_byte_units: 1,
            index_byte_units: 1,
            c_cell: 64,
            c_idx: 64,
            max_lifetime,
        }
    }
}
