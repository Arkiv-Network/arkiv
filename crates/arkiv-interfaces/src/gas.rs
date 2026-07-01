//! Where gas pricing will live.
//!
//! For now just the trait and a zero-cost placeholder, so an executor can carry a
//! cost model before the real prices exist. The real numbers (the reference impl
//! uses e.g. `G_CREATE = 80_000`, `G_BYTE = 16`) will land here.

use crate::execution::Op;
use crate::primitives::Gas;
use crate::query::QueryStats;

/// Prices operations and queries. Deterministic.
pub trait CostModel {
    /// The gas for one operation. The [`Op`] carries all the schedule needs — its
    /// kind, payload size, and attribute counts.
    fn op_cost(&self, op: &Op) -> Gas;

    /// The gas for a query, from the work it did.
    fn query_cost(&self, stats: &QueryStats) -> Gas;
}

/// A cost model that charges nothing — a stand-in until the real schedule exists.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaceholderCost;

impl CostModel for PlaceholderCost {
    fn op_cost(&self, _op: &Op) -> Gas {
        // TODO: real per-op schedule.
        0
    }

    fn query_cost(&self, _stats: &QueryStats) -> Gas {
        // TODO: real per-query schedule.
        0
    }
}
