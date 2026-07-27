//! Where gas pricing lives.
//!
//! [`PlaceholderCost`] carries a **provisional, non-zero** schedule so gas
//! accounting is exercised end to end (out-of-gas actually triggers) before the
//! final numbers are set. The constants below are rough stand-ins — mirroring the
//! reference impl's `G_CREATE = 80_000`, `G_BYTE = 16`, per-attribute pricing —
//! not the frozen schedule. Expect them to change.

use crate::entity::Attribute;
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

// --- Provisional schedule (non-final placeholder numbers) --------------------

/// Base cost to create an entity.
const G_CREATE: Gas = 80_000;
/// Base cost to update an entity's contents.
const G_UPDATE: Gas = 40_000;
/// Base cost to extend an entity's expiry.
const G_EXTEND_EXPIRY: Gas = 10_000;
/// Base cost to transfer ownership.
const G_TRANSFER: Gas = 20_000;
/// Base cost to delete an entity.
const G_DELETE: Gas = 10_000;
/// Base cost to expire an entity.
const G_EXPIRE: Gas = 5_000;
/// Per byte of `content_type` + `payload` a write carries.
const G_BYTE: Gas = 16;
/// Per attribute a write carries (index maintenance).
const G_ATTRIBUTE: Gas = 5_000;
/// Per entity a query scans.
const G_SCANNED: Gas = 100;
/// Per index lookup a query makes.
const G_INDEX_LOOKUP: Gas = 2_000;
/// Per entity a query returns.
const G_RETURNED: Gas = 500;

/// The size-dependent surcharge on a write: bytes stored plus attributes indexed.
fn data_cost(content_type: &[u8], payload: &[u8], attributes: &[Attribute]) -> Gas {
    let bytes = (content_type.len() as Gas).saturating_add(payload.len() as Gas);
    bytes
        .saturating_mul(G_BYTE)
        .saturating_add((attributes.len() as Gas).saturating_mul(G_ATTRIBUTE))
}

/// A **provisional** cost model with non-zero, non-final numbers. Enough to
/// exercise gas accounting (metering, out-of-gas) until the real schedule lands.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaceholderCost;

impl CostModel for PlaceholderCost {
    fn op_cost(&self, op: &Op) -> Gas {
        match op {
            Op::Create {
                content_type,
                payload,
                attributes,
                ..
            } => G_CREATE.saturating_add(data_cost(content_type, payload, attributes)),
            Op::Update {
                content_type,
                payload,
                attributes,
                ..
            } => G_UPDATE.saturating_add(data_cost(content_type, payload, attributes)),
            Op::ExtendExpiry { .. } => G_EXTEND_EXPIRY,
            Op::Transfer { .. } => G_TRANSFER,
            Op::Delete { .. } => G_DELETE,
            Op::Expire { .. } => G_EXPIRE,
        }
    }

    fn query_cost(&self, stats: &QueryStats) -> Gas {
        stats
            .index_lookups
            .saturating_mul(G_INDEX_LOOKUP)
            .saturating_add(stats.entities_scanned.saturating_mul(G_SCANNED))
            .saturating_add(stats.entities_returned.saturating_mul(G_RETURNED))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn base_op_costs_are_non_zero() {
        let key = [0u8; 32];
        let empty = || {
            (
                alloc::vec::Vec::new(),
                alloc::vec::Vec::new(),
                alloc::vec::Vec::new(),
            )
        };
        let (ct, pl, at) = empty();
        assert_eq!(
            PlaceholderCost.op_cost(&Op::Create {
                key,
                expires_at: 1,
                content_type: ct,
                payload: pl,
                attributes: at,
            }),
            G_CREATE
        );
        assert_eq!(PlaceholderCost.op_cost(&Op::Delete { key }), G_DELETE);
        assert_eq!(
            PlaceholderCost.op_cost(&Op::Transfer {
                key,
                new_owner: [0u8; 20],
            }),
            G_TRANSFER
        );
    }

    #[test]
    fn writes_charge_for_size_and_attributes() {
        let key = [0u8; 32];
        let one_attr = vec![Attribute::new(
            b"k".to_vec(),
            crate::entity::AttributeValue::Str("v".into()),
        )];
        let cost = PlaceholderCost.op_cost(&Op::Create {
            key,
            expires_at: 1,
            content_type: b"ab".to_vec(), // 2 bytes
            payload: b"cde".to_vec(),     // 3 bytes
            attributes: one_attr,
        });
        // base + 5 bytes * G_BYTE + 1 attr * G_ATTRIBUTE
        assert_eq!(cost, G_CREATE + 5 * G_BYTE + G_ATTRIBUTE);
    }

    #[test]
    fn query_cost_sums_the_work() {
        let stats = QueryStats {
            entities_scanned: 4,
            entities_returned: 2,
            index_lookups: 1,
            gas_used: 0,
            partial: false,
        };
        assert_eq!(
            PlaceholderCost.query_cost(&stats),
            G_INDEX_LOOKUP + 4 * G_SCANNED + 2 * G_RETURNED
        );
    }
}
