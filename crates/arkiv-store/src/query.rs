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
    /// Predicates evaluated.
    index_lookups: u64,
    /// Index entries visited by the predicate walks.
    entries: u64,
    /// Elements consumed by the merges.
    merge_steps: u64,
}

impl Work {
    /// Record a predicate that produced `keys` and hand them on.
    fn scanned(&mut self, keys: Vec<EntityAddress>) -> Vec<EntityAddress> {
        self.index_lookups += 1;
        self.entries += keys.len() as u64;
        keys
    }

    /// Record a merge of two streams.
    fn merged(&mut self, a: &[EntityAddress], b: &[EntityAddress]) {
        self.merge_steps += (a.len() + b.len()) as u64;
    }
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
            index_entries_scanned: work.entries,
            merge_steps: work.merge_steps,
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
            let keys: Vec<EntityAddress> = view.entity_keys()?.collect::<Result<_, _>>()?;
            Ok(work.scanned(keys))
        }
        Query::Eq { key, value } => {
            if !is_indexed(value.attr_type()) {
                return Ok(work.scanned(Vec::new()));
            }
            // Equal values share one prefix, so the keys are already ascending.
            let keys = view.equal(&attr_bytes(key), value)?;
            Ok(work.scanned(keys))
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
            let AttributeValue::Str(prefix) = value else {
                return Ok(work.scanned(Vec::new()));
            };
            let keys = sorted(view.prefixed(&attr_bytes(key), prefix)?);
            Ok(work.scanned(keys))
        }
        Query::And(left, right) => {
            let l = eval(view, left, work)?;
            if l.is_empty() {
                // Short-circuit: the right side is neither scanned nor merged.
                return Ok(l);
            }
            let r = eval(view, right, work)?;
            work.merged(&l, &r);
            Ok(intersect(&l, &r))
        }
        Query::Or(left, right) => {
            let l = eval(view, left, work)?;
            let r = eval(view, right, work)?;
            work.merged(&l, &r);
            Ok(union(&l, &r))
        }
        Query::Not(inner) => {
            // The complement walks the live set: the entities trie.
            let all: Vec<EntityAddress> = view.entity_keys()?.collect::<Result<_, _>>()?;
            let all = work.scanned(all);
            let matching = eval(view, inner, work)?;
            work.merged(&all, &matching);
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
    let ty = match (low, high) {
        (Bound::Included(v) | Bound::Excluded(v), _)
        | (_, Bound::Included(v) | Bound::Excluded(v)) => v.attr_type(),
        (Bound::Unbounded, Bound::Unbounded) => return Ok(work.scanned(Vec::new())),
    };
    if !is_indexed(ty) {
        return Ok(work.scanned(Vec::new()));
    }
    let mut keys = view.range(&attr_bytes(key), ty, low, high)?;
    keys.sort_unstable();
    keys.dedup();
    Ok(work.scanned(keys))
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
    use crate::roots::DbRoots;
    use crate::view::{DbChanges, commit_changes};
    use arkiv_interfaces::entity::{Attribute, Entity};
    use arkiv_interfaces::query::AnnotKey;
    use arkiv_trie::MemNodeStore;

    /// Entities 1..=n with `a = i` and `b = i % 3`.
    fn populated(n: u8) -> (MemNodeStore, DbRoots) {
        let mut store = MemNodeStore::new();
        let mut changes = DbChanges::default();
        for i in 1..=n {
            changes.entities.insert(
                [i; 32],
                Some(Entity {
                    key: [i; 32],
                    expires_at: 1000,
                    attributes: vec![
                        Attribute::new(b"a".to_vec(), AttributeValue::U64(i as u64)),
                        Attribute::new(b"b".to_vec(), AttributeValue::U64((i % 3) as u64)),
                    ],
                    ..Entity::default()
                }),
            );
        }
        let roots = commit_changes(&mut store, DbRoots::EMPTY, &changes).unwrap();
        (store, roots)
    }

    fn user(name: &str) -> AnnotKey {
        AnnotKey::User(name.to_string())
    }

    fn page() -> PageParams {
        PageParams {
            page_size: 100,
            cursor: None,
        }
    }

    /// A predicate costs the entities that satisfy it; a merge costs the sum
    /// of its inputs; a short-circuited `AND` costs nothing on the right.
    #[test]
    fn counters_follow_the_scans_and_merges() {
        let (store, roots) = populated(9);
        let view = DbView::at(&store, roots);

        // a > 3: entities 4..=9, six entries, one predicate, no merge.
        let q = Query::Gt {
            key: user("a"),
            value: AttributeValue::U64(3),
        };
        let m = evaluate_page(&view, &q, page()).unwrap();
        assert_eq!(m.stats.entities_scanned, 6);
        assert_eq!(m.stats.index_entries_scanned, 6);
        assert_eq!(m.stats.index_lookups, 1);
        assert_eq!(m.stats.merge_steps, 0);

        // a > 3 AND b == 0: six entries + three entries, merged (6 + 3).
        let q = Query::And(
            Box::new(q.clone()),
            Box::new(Query::Eq {
                key: user("b"),
                value: AttributeValue::U64(0),
            }),
        );
        let m = evaluate_page(&view, &q, page()).unwrap();
        assert_eq!(m.stats.entities_scanned, 2, "6 and 9");
        assert_eq!(m.stats.index_entries_scanned, 9);
        assert_eq!(m.stats.index_lookups, 2);
        assert_eq!(m.stats.merge_steps, 9);

        // An empty left side short-circuits: the right side is never scanned.
        let q = Query::And(
            Box::new(Query::Gt {
                key: user("a"),
                value: AttributeValue::U64(100),
            }),
            Box::new(Query::Eq {
                key: user("b"),
                value: AttributeValue::U64(0),
            }),
        );
        let m = evaluate_page(&view, &q, page()).unwrap();
        assert_eq!(m.stats.index_entries_scanned, 0);
        assert_eq!(m.stats.index_lookups, 1);
        assert_eq!(m.stats.merge_steps, 0);

        // NOT b == 0: walks all nine, merges 9 + 3.
        let q = Query::Not(Box::new(Query::Eq {
            key: user("b"),
            value: AttributeValue::U64(0),
        }));
        let m = evaluate_page(&view, &q, page()).unwrap();
        assert_eq!(m.stats.entities_scanned, 6);
        assert_eq!(m.stats.index_entries_scanned, 12);
        assert_eq!(m.stats.merge_steps, 12);
    }

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
