//! Arkiv's filters as GolemDB's.
//!
//! Arkiv writes an ordered DNF (an OR of AND-groups of predicates); GolemDB
//! takes a tree of `And` and `Or` over leaves, so the shape maps directly.
//! What GolemDB cannot evaluate yet is a negated predicate: those are left out
//! of the tree and checked against the matched records afterwards, so a group
//! needs at least one positive predicate to narrow the search.

use std::ops::Bound;

use arkiv_interfaces::store::{AndGroup, Cell, CompareOp, Filter, Predicate, StoreError};
use golemdb_api as golemdb;
use golemdb_api::{Bounds, CellName, CellNameRef, EqValue};

use crate::convert::golemdb_cell;

/// A filter GolemDB can run, and whether its matches still need the negated
/// predicates applied.
pub(crate) struct Lowered {
    pub(crate) filter: golemdb::Filter,
    pub(crate) negations: bool,
}

pub(crate) fn lower(filter: &Filter) -> Result<Lowered, StoreError> {
    // An empty filter matches every record, which GolemDB has no leaf for.
    if filter.0.is_empty() {
        return Err(StoreError::NotImplemented);
    }
    let mut negations = false;
    let mut groups = Vec::with_capacity(filter.0.len());
    for group in &filter.0 {
        negations |= group.0.iter().any(|predicate| predicate.negated);
        groups.push(lower_group(group)?);
    }
    let filter = if groups.len() == 1 {
        groups.pop().expect("one group")
    } else {
        golemdb::Filter::Or(groups)
    };
    Ok(Lowered { filter, negations })
}

fn lower_group(group: &AndGroup) -> Result<golemdb::Filter, StoreError> {
    let mut leaves = Vec::with_capacity(group.0.len());
    for predicate in &group.0 {
        if !predicate.op.permitted_by(predicate.type_id.index_class()) {
            return Err(StoreError::InvalidQuery);
        }
        if !predicate.negated {
            leaves.push(leaf(predicate)?);
        }
    }
    match leaves.len() {
        // Only negations: every record is a candidate, as for an empty filter.
        0 => Err(StoreError::NotImplemented),
        1 => Ok(leaves.pop().expect("one leaf")),
        _ => Ok(golemdb::Filter::And(leaves)),
    }
}

fn leaf(predicate: &Predicate) -> Result<golemdb::Filter, StoreError> {
    let field = CellName::from(CellNameRef::raw(predicate.cell.as_bytes()));
    if predicate.op == CompareOp::Prefix {
        let prefix =
            String::from_utf8(predicate.value.clone()).map_err(|_| StoreError::InvalidQuery)?;
        return Ok(golemdb::Filter::Prefix { field, prefix });
    }
    let value = golemdb_cell(&Cell::attribute(predicate.type_id, predicate.value.clone()))?;
    let cell_type = value.cell_type();
    let range = |lower, upper| {
        Bounds::try_new(cell_type, lower, upper)
            .map(|bounds| golemdb::Filter::Range {
                field: field.clone(),
                bounds,
            })
            .map_err(|_| StoreError::InvalidQuery)
    };
    match predicate.op {
        CompareOp::Eq => EqValue::try_from(value)
            .map(|value| golemdb::Filter::Eq {
                field: field.clone(),
                value,
            })
            .map_err(|_| StoreError::InvalidQuery),
        CompareOp::Lt => range(Bound::Unbounded, Bound::Excluded(value)),
        CompareOp::Lte => range(Bound::Unbounded, Bound::Included(value)),
        CompareOp::Gt => range(Bound::Excluded(value), Bound::Unbounded),
        CompareOp::Gte => range(Bound::Included(value), Bound::Unbounded),
        CompareOp::Prefix => unreachable!("handled above"),
    }
}
