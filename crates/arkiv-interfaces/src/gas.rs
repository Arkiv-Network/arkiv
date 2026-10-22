//! Where gas pricing lives.
//!
//! [`PlaceholderCost`] carries a **provisional, non-zero** schedule so gas
//! accounting is exercised end to end (out-of-gas actually triggers) before the
//! final numbers are set. The constants below are rough stand-ins — mirroring the
//! reference impl's `G_CREATE = 80_000`, `G_BYTE = 16`, per-attribute pricing —
//! not the frozen schedule. Expect them to change.

use crate::entity::Attribute;
use crate::execution::{AttributeMutation, Op};
use crate::primitives::Gas;
use crate::query::QueryStats;

/// Prices operations and queries. Deterministic.
///
/// This is the **schedule** building block — a pure price list. The question
/// "how much for operation `o`?" is asked of the
/// [`StateManager`](crate::statemanager::StateManager) (its `get_operation_cost`),
/// which may consult committed state on the way to an answer; a schedule like
/// this is what it answers with.
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
/// Base cost to patch an entity's contents.
const G_PATCH: Gas = 40_000;
/// Base cost to extend an entity's expiry.
const G_EXTEND_EXPIRY: Gas = 10_000;
/// Base cost to transfer ownership.
const G_TRANSFER: Gas = 20_000;
/// Base cost to delete an entity.
const G_DELETE: Gas = 10_000;
/// Per byte of attribute value a write carries.
const G_BYTE: Gas = 16;
/// Per indexed attribute a write carries (index maintenance). Unindexed
/// attributes do not pay it — they have no index to maintain.
const G_ATTRIBUTE: Gas = 5_000;

/// Protocol cost of physically purging one expired entity with
/// `indexed_attributes` index entries to remove.
pub const fn purge_cost(indexed_attributes: usize) -> Gas {
    G_DELETE.saturating_add(G_ATTRIBUTE.saturating_mul(indexed_attributes as Gas))
}
/// Per entity a query scans.
const G_SCANNED: Gas = 100;
/// Per index lookup a query makes.
const G_INDEX_LOOKUP: Gas = 2_000;
/// Per entity a query returns.
const G_RETURNED: Gas = 500;

/// The size-dependent surcharge on a write: bytes stored plus attributes indexed.
fn data_cost(attributes: &[Attribute]) -> Gas {
    attributes.iter().fold(0, |acc, a| {
        let bytes = (a.value.encode().len() as Gas).saturating_mul(G_BYTE);
        let index = if a.indexed { G_ATTRIBUTE } else { 0 };
        acc.saturating_add(bytes).saturating_add(index)
    })
}

/// A patch costs its *mutations*, not the entity it touches.
///
/// This is the point of the op: `k` mutations cost `k` touches, with no
/// O(entity) floor. A one-attribute patch of a large entity is cheap, so
/// callers have no incentive to batch unrelated writes to amortise a floor —
/// which is also what lets disjoint patches compose instead of racing.
///
/// An indexed set costs a touch. So does an unset (tombstone): the attribute it
/// removes may have been indexed, and removing an index entry is work. An
/// unindexed set pays only for its bytes.
fn mutation_cost(mutations: &[AttributeMutation]) -> Gas {
    mutations.iter().fold(0, |acc, m| {
        let bytes = m.value.as_ref().map_or(0, |v| v.encode().len() as Gas);
        let touch = if m.indexed || m.value.is_none() {
            G_ATTRIBUTE
        } else {
            0
        };
        acc.saturating_add(touch)
            .saturating_add(bytes.saturating_mul(G_BYTE))
    })
}

/// A **provisional** cost model with non-zero, non-final numbers. Enough to
/// exercise gas accounting (metering, out-of-gas) until the real schedule lands.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaceholderCost;

impl CostModel for PlaceholderCost {
    fn op_cost(&self, op: &Op) -> Gas {
        match op {
            Op::Create { attributes, .. } => G_CREATE.saturating_add(data_cost(attributes)),
            Op::Patch { mutations, .. } => G_PATCH.saturating_add(mutation_cost(mutations)),
            Op::ExtendExpiry { .. } => G_EXTEND_EXPIRY,
            Op::Transfer { .. } => G_TRANSFER,
            Op::Delete { .. } => G_DELETE,
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
    use crate::entity::{AttributeValue, CreationFlags};
    use alloc::vec;

    #[test]
    fn writes_charge_for_size_and_attributes() {
        let key = [0u8; 32];
        let attributes = vec![
            Attribute::new(b"k".to_vec(), AttributeValue::Str("ab".into())), // 2 bytes
            Attribute::unindexed(b"b".to_vec(), AttributeValue::Bytes(b"cde".to_vec())), // 3 bytes
        ];
        let cost = PlaceholderCost.op_cost(&Op::Create {
            key,
            expires_at: 1,
            creation_flags: CreationFlags::NONE,
            attributes,
        });
        // base + 5 bytes * G_BYTE + 1 indexed attr * G_ATTRIBUTE
        assert_eq!(cost, G_CREATE + 5 * G_BYTE + G_ATTRIBUTE);
    }

    #[test]
    fn purge_cost_is_delete_plus_attributes() {
        assert_eq!(purge_cost(0), G_DELETE);
        assert_eq!(purge_cost(7), G_DELETE + 7 * G_ATTRIBUTE);
    }

    /// A patch is priced on what it changes, not on the entity it changes —
    /// so cost scales with the mutation count and nothing else.
    #[test]
    fn patch_cost_scales_with_mutations_not_entity_size() {
        let key = [0u8; 32];
        let patch = |mutations| PlaceholderCost.op_cost(&Op::Patch { key, mutations });

        // One set: base + one touch + its value bytes ("v" encodes to 1 byte).
        let one = patch(vec![AttributeMutation::set(
            b"k".to_vec(),
            AttributeValue::Str("v".into()),
        )]);
        assert_eq!(one, G_PATCH + G_ATTRIBUTE + G_BYTE);

        // Two sets cost exactly one more touch than one — linear, no floor.
        let two = patch(vec![
            AttributeMutation::set(b"k".to_vec(), AttributeValue::Str("v".into())),
            AttributeMutation::set(b"j".to_vec(), AttributeValue::Str("w".into())),
        ]);
        assert_eq!(two - one, G_ATTRIBUTE + G_BYTE);

        // An unset carries no bytes but still costs a touch: removing an index
        // entry is work.
        assert_eq!(
            patch(vec![AttributeMutation::unset(b"k".to_vec())]),
            G_PATCH + G_ATTRIBUTE
        );

        // An unindexed set has no index entry to move: bytes only.
        assert_eq!(
            patch(vec![AttributeMutation::set_unindexed(
                b"k".to_vec(),
                AttributeValue::Str("v".into()),
            )]),
            G_PATCH + G_BYTE
        );
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
