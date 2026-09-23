//! Evaluate a [`Query`] against a [`DbView`] to the sorted set of matching
//! entity keys. Every predicate yields keys ascending; `And`, `Or` and `Not`
//! are sorted-set operations on them.

use core::ops::Bound;

use arkiv_interfaces::entity::AttributeValue;
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::query::{PageParams, Query, QueryMatches, QueryStats};
use arkiv_trie::NodeReader;

use crate::annotations::{attr_bytes, is_indexed};
use crate::view::{DbView, StoreError};

/// Work counters for one evaluation.
#[derive(Debug, Default, Clone, Copy)]
struct Work {
    index_lookups: u64,
}

/// The matching entity keys, ascending.
pub fn evaluate<S: NodeReader>(
    view: &DbView<'_, S>,
    query: &Query,
) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
    let mut work = Work::default();
    eval(view, query, &mut work)
}

/// One page of matches, plus the statistics. The cursor is the number of
/// matches to skip: pages are stable because a view is one immutable root.
pub fn evaluate_page<S: NodeReader>(
    view: &DbView<'_, S>,
    query: &Query,
    page: PageParams,
) -> Result<QueryMatches, StoreError<S::Error>> {
    let mut work = Work::default();
    let all = eval(view, query, &mut work)?;
    let skip = page.cursor.unwrap_or(0) as usize;
    let page_size = page.page_size as usize;
    let keys: Vec<EntityAddress> = all.iter().skip(skip).take(page_size).copied().collect();
    let end = skip + keys.len();
    let next_cursor = (end < all.len()).then_some(end as u64);
    let returned = keys.len() as u64;
    Ok(QueryMatches {
        keys,
        next_cursor,
        stats: QueryStats {
            entities_scanned: all.len() as u64,
            entities_returned: returned,
            index_lookups: work.index_lookups,
            gas_used: 0,
            partial: false,
        },
    })
}

fn eval<S: NodeReader>(
    view: &DbView<'_, S>,
    query: &Query,
    work: &mut Work,
) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
    let sorted = |mut keys: Vec<EntityAddress>| {
        keys.sort_unstable();
        keys.dedup();
        keys
    };
    match query {
        Query::All => {
            work.index_lookups += 1;
            view.entity_keys()?.collect()
        }
        Query::Eq { key, value } => {
            work.index_lookups += 1;
            if !is_indexed(value.attr_type()) {
                return Ok(Vec::new());
            }
            // Equal values share one prefix, so the keys are already ascending.
            view.equal(&attr_bytes(key), value)
        }
        Query::Gt { key, value } => {
            range(view, key, Bound::Excluded(value), Bound::Unbounded, work)
        }
        Query::Gte { key, value } => {
            range(view, key, Bound::Included(value), Bound::Unbounded, work)
        }
        Query::Lt { key, value } => {
            range(view, key, Bound::Unbounded, Bound::Excluded(value), work)
        }
        Query::Lte { key, value } => {
            range(view, key, Bound::Unbounded, Bound::Included(value), work)
        }
        Query::StartsWith { key, value } => {
            work.index_lookups += 1;
            let AttributeValue::Str(prefix) = value else {
                return Ok(Vec::new());
            };
            Ok(sorted(view.prefixed(&attr_bytes(key), prefix)?))
        }
        Query::And(left, right) => {
            let l = eval(view, left, work)?;
            if l.is_empty() {
                return Ok(l);
            }
            let r = eval(view, right, work)?;
            Ok(intersect(&l, &r))
        }
        Query::Or(left, right) => {
            let l = eval(view, left, work)?;
            let r = eval(view, right, work)?;
            Ok(union(&l, &r))
        }
        Query::Not(inner) => {
            let all: Vec<EntityAddress> = view.entity_keys()?.collect::<Result<_, _>>()?;
            work.index_lookups += 1;
            let matching = eval(view, inner, work)?;
            Ok(difference(&all, &matching))
        }
    }
}

fn range<S: NodeReader>(
    view: &DbView<'_, S>,
    key: &arkiv_interfaces::query::AnnotKey,
    low: Bound<&AttributeValue>,
    high: Bound<&AttributeValue>,
    work: &mut Work,
) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
    work.index_lookups += 1;
    let ty = match (low, high) {
        (Bound::Included(v) | Bound::Excluded(v), _)
        | (_, Bound::Included(v) | Bound::Excluded(v)) => v.attr_type(),
        (Bound::Unbounded, Bound::Unbounded) => return Ok(Vec::new()),
    };
    if !is_indexed(ty) {
        return Ok(Vec::new());
    }
    let mut keys = view.range(&attr_bytes(key), ty, low, high)?;
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

pub(crate) fn intersect(a: &[EntityAddress], b: &[EntityAddress]) -> Vec<EntityAddress> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
            core::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

pub(crate) fn union(a: &[EntityAddress], b: &[EntityAddress]) -> Vec<EntityAddress> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            core::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            core::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

pub(crate) fn difference(a: &[EntityAddress], b: &[EntityAddress]) -> Vec<EntityAddress> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for x in a {
        while j < b.len() && b[j] < *x {
            j += 1;
        }
        if j < b.len() && b[j] == *x {
            continue;
        }
        out.push(*x);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_operations_on_sorted_keys() {
        let a = vec![[1u8; 32], [3; 32], [5; 32]];
        let b = vec![[2u8; 32], [3; 32], [5; 32], [7; 32]];
        assert_eq!(intersect(&a, &b), vec![[3; 32], [5; 32]]);
        assert_eq!(
            union(&a, &b),
            vec![[1; 32], [2; 32], [3; 32], [5; 32], [7; 32]]
        );
        assert_eq!(difference(&a, &b), vec![[1; 32]]);
        assert_eq!(difference(&b, &a), vec![[2; 32], [7; 32]]);
    }
}
