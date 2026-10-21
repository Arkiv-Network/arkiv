//! Arkiv's store [`Query`] and [`Filter`] as a GolemDB
//! [`QueryOp`](golemdb::QueryOp) and [`CountOp`](golemdb::CountOp), as far as
//! GolemDB can express them, and GolemDB's page back as a [`QueryResult`].
//!
//! A GolemDB filter is a single leaf, with no AND, OR or NOT. So only a filter
//! of one predicate, or an OR of equalities on one cell, has a counterpart.
//! Anything else that would change the answer is
//! [`StoreError::NotImplemented`], as the rest of the adapter reports what
//! GolemDB lacks. An offset of 0 is no offset, so it is dropped.
//!
//! One difference is not in the query at all: unsorted, and as a sort's
//! tie-break, GolemDB returns records in record ID order and Arkiv in key
//! order.

use std::ops::Bound::{Excluded, Included, Unbounded};

use arkiv_interfaces::store::{
    AndGroup, Budget, Cell, CellKind, CompareOp, Filter, Predicate, Query, QueryResult, Sort,
    SortDirection, StoreError,
};
use golemdb_api as golemdb;
use golemdb_api::{Bounds, CellType, EqValue, InValues};

use super::{arkiv_record, cell_type, golemdb_cell};

/// `query` as a GolemDB op, capped by `budget`. See the module docs for what
/// has no counterpart.
pub(crate) fn query_op(
    query: &Query,
    budget: Option<Budget>,
) -> Result<golemdb::QueryOp, StoreError> {
    let filter = golemdb_filter(&query.filter)?;
    let sort = query
        .sort
        .as_ref()
        .map(|sort| sort_key(sort, &filter))
        .transpose()?;
    let mut op = golemdb::QueryOp::from_filter(filter)
        .sort(sort)
        .limit(query.page.limit)
        .budget(golemdb_budget(budget));
    if query.page.offset > 0 {
        op = op.offset(query.page.offset);
    }
    if let Some(names) = &query.projection {
        op = op.only(names.iter().map(String::as_bytes));
    }
    if query.total_matched {
        op = op.with_total();
    }
    Ok(op)
}

/// `filter` as a GolemDB count, capped by `budget`.
pub(crate) fn count_op(
    filter: &Filter,
    budget: Option<Budget>,
) -> Result<golemdb::CountOp, StoreError> {
    Ok(golemdb::CountOp::filter(golemdb_filter(filter)?).budget(golemdb_budget(budget)))
}

/// Arkiv's budget as GolemDB's. No budget is an unbudgeted call, which GolemDB
/// spells `Unlimited`. A cap carries over as the same number: GolemDB prices
/// no work yet, so its units and Arkiv's cost units are not reconciled.
fn golemdb_budget(budget: Option<Budget>) -> golemdb::Budget {
    match budget {
        Some(Budget(cap)) => golemdb::Budget::Limited(cap),
        None => golemdb::Budget::Unlimited,
    }
}

/// GolemDB's page as Arkiv's result. It was evaluated at the commit asked for,
/// or at the head when none was, so that commit is dropped.
pub(crate) fn arkiv_query_result(page: &golemdb::QueryPage) -> Result<QueryResult, StoreError> {
    Ok(QueryResult {
        records: page
            .records
            .iter()
            .map(arkiv_record)
            .collect::<Result<_, _>>()?,
        total_matched: page.total_matched,
    })
}

/// Arkiv's DNF as one GolemDB leaf.
///
/// Every predicate is checked against its type's index first, as Arkiv checks
/// it: a range on a `str` is invalid even though GolemDB could run it.
fn golemdb_filter(filter: &Filter) -> Result<golemdb::Filter, StoreError> {
    for predicate in filter.0.iter().flat_map(|group| &group.0) {
        if !predicate.op.permitted_by(predicate.type_id.index_class()) {
            return Err(StoreError::InvalidQuery);
        }
    }
    // One predicate per group, not negated. An empty group matches every
    // record, which GolemDB cannot ask for either.
    let leaves = filter
        .0
        .iter()
        .map(|AndGroup(group)| match group.as_slice() {
            [predicate] if !predicate.negated => Ok(predicate),
            _ => Err(StoreError::NotImplemented),
        })
        .collect::<Result<Vec<_>, _>>()?;
    match leaves.as_slice() {
        // No group: every record.
        [] => Err(StoreError::NotImplemented),
        [predicate] => leaf(predicate),
        [first, ..] => any_of(first, &leaves),
    }
}

/// One predicate as a leaf. `KEY` and `BYTES32` are one GolemDB type, so a
/// leaf of either matches both.
fn leaf(predicate: &Predicate) -> Result<golemdb::Filter, StoreError> {
    let field = cell_name(&predicate.cell);
    let bounds = |lower, upper| {
        Bounds::try_new(cell_type(predicate.type_id)?, lower, upper).map_err(invalid_query)
    };
    Ok(match predicate.op {
        CompareOp::Eq => golemdb::Filter::Eq {
            field,
            value: EqValue::try_from(operand(predicate)?).map_err(invalid_query)?,
        },
        CompareOp::Lt => golemdb::Filter::Range {
            field,
            bounds: bounds(Unbounded, Excluded(operand(predicate)?))?,
        },
        CompareOp::Lte => golemdb::Filter::Range {
            field,
            bounds: bounds(Unbounded, Included(operand(predicate)?))?,
        },
        CompareOp::Gt => golemdb::Filter::Range {
            field,
            bounds: bounds(Excluded(operand(predicate)?), Unbounded)?,
        },
        CompareOp::Gte => golemdb::Filter::Range {
            field,
            bounds: bounds(Included(operand(predicate)?), Unbounded)?,
        },
        CompareOp::Prefix => golemdb::Filter::Prefix {
            field,
            // GolemDB's prefix is text, so a prefix that ends inside a
            // character cannot be asked.
            prefix: String::from_utf8(predicate.value.clone())
                .map_err(|_| StoreError::NotImplemented)?,
        },
    })
}

/// An OR of equalities on one cell, of one type, is GolemDB's `In`. Any other
/// OR has no counterpart.
fn any_of(first: &Predicate, predicates: &[&Predicate]) -> Result<golemdb::Filter, StoreError> {
    let alike = |predicate: &&Predicate| {
        predicate.op == CompareOp::Eq
            && predicate.cell == first.cell
            && predicate.type_id == first.type_id
    };
    if !predicates.iter().all(alike) {
        return Err(StoreError::NotImplemented);
    }
    let values = predicates
        .iter()
        .map(|predicate| operand(predicate))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(golemdb::Filter::In {
        field: cell_name(&first.cell),
        values: InValues::try_from(values).map_err(invalid_query)?,
    })
}

/// Arkiv sorts by whatever type the cell holds, GolemDB by the cell read as one
/// type. The two agree when the filter holds the cell to one type, since every
/// match then holds that type there. Otherwise there is no type to give.
fn sort_key(sort: &Sort, filter: &golemdb::Filter) -> Result<golemdb::SortKey, StoreError> {
    let ty = held_type(filter, &sort.cell).ok_or(StoreError::NotImplemented)?;
    Ok(match sort.direction {
        SortDirection::Ascending => golemdb::SortKey::asc(&sort.cell, ty),
        SortDirection::Descending => golemdb::SortKey::desc(&sort.cell, ty),
    })
}

/// The type `filter` holds the cell `name` to, if its leaf is on that cell.
fn held_type(filter: &golemdb::Filter, name: &str) -> Option<CellType> {
    let (field, ty) = match filter {
        golemdb::Filter::Eq { field, value } => (field, value.cell_type()),
        golemdb::Filter::In { field, values } => (field, values.as_slice().first()?.cell_type()),
        golemdb::Filter::Range { field, bounds } => (field, bounds.cell_type()),
        golemdb::Filter::Prefix { field, .. } => (field, CellType::Str),
        _ => return None,
    };
    (field.as_bytes() == name.as_bytes()).then_some(ty)
}

/// A predicate's value encoded as [`golemdb_cell`] writes a cell, so a leaf
/// finds what was stored. A field, which is what a leaf's values are.
fn operand(predicate: &Predicate) -> Result<golemdb::CellValue, StoreError> {
    golemdb_cell(&Cell {
        kind: CellKind::Field,
        type_id: predicate.type_id,
        value: predicate.value.clone(),
    })
}

/// Taken raw: GolemDB checks the name when the op runs.
fn cell_name(name: &str) -> golemdb::CellName {
    golemdb::CellNameRef::raw(name.as_bytes()).into()
}

/// Values a leaf cannot hold, such as `bytes`, which has no index term.
fn invalid_query(_: golemdb::ValuesError) -> StoreError {
    StoreError::InvalidQuery
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GolemDbConfig, GolemDbStore};
    use arkiv_interfaces::store::{Page, RecordKey, Store, TypeId};
    use golemdb_api::marker::{I32, Str, U64};
    use golemdb_api::{Api, CellLimits, HashAlgorithm, Width};
    use tempfile::TempDir;

    fn predicate(cell: &str, op: CompareOp, type_id: TypeId, value: &[u8]) -> Predicate {
        Predicate {
            cell: cell.into(),
            op,
            type_id,
            value: value.to_vec(),
            negated: false,
        }
    }

    fn u64_is(cell: &str, op: CompareOp, value: u64) -> Predicate {
        predicate(cell, op, TypeId::U64, &value.to_be_bytes())
    }

    fn i32_is(cell: &str, op: CompareOp, value: i32) -> Predicate {
        predicate(cell, op, TypeId::I32, &value.to_be_bytes())
    }

    fn query(groups: Vec<Vec<Predicate>>) -> Query {
        Query {
            filter: Filter(groups.into_iter().map(AndGroup).collect()),
            page: Page {
                offset: 0,
                limit: 10,
            },
            ..Query::default()
        }
    }

    #[test]
    fn one_equality_maps_every_field() {
        let mut arkiv = query(vec![vec![predicate(
            "color",
            CompareOp::Eq,
            TypeId::STR,
            b"blue",
        )]]);
        arkiv.page = Page {
            offset: 20,
            limit: 10,
        };
        arkiv.projection = Some(vec!["size".into(), "color".into()]);
        arkiv.total_matched = true;

        let expected = golemdb::QueryOp::from_filter(golemdb::Filter::Eq {
            field: cell_name("color"),
            value: EqValue::of(Str, "blue"),
        })
        .only(["color", "size"])
        .limit(10)
        .offset(20)
        .with_total()
        .budget(golemdb::Budget::Limited(1_000));
        assert_eq!(query_op(&arkiv, Some(Budget(1_000))), Ok(expected));
    }

    #[test]
    fn no_budget_is_unlimited() {
        assert_eq!(golemdb_budget(None), golemdb::Budget::Unlimited);
    }

    /// The same number, unscaled, up to the largest, which is still a cap
    /// rather than unlimited.
    #[test]
    fn a_cap_carries_over_as_the_same_number() {
        for cap in [1, 7, 1_000_000, u64::MAX] {
            assert_eq!(
                golemdb_budget(Some(Budget(cap))),
                golemdb::Budget::Limited(cap),
                "{cap}"
            );
        }
    }

    /// A zero budget pays for no work, the opposite of no budget.
    #[test]
    fn a_zero_cap_stays_a_cap() {
        assert_eq!(golemdb_budget(Some(Budget(0))), golemdb::Budget::Limited(0));
    }

    /// No budget is an unbudgeted call; a cap carries over as the same number.
    #[test]
    fn a_budget_caps_a_query_and_a_count_alike() {
        let arkiv = query(vec![vec![u64_is("n", CompareOp::Eq, 1)]]);
        for (budget, expected) in [
            (None, golemdb::Budget::Unlimited),
            (Some(Budget(7)), golemdb::Budget::Limited(7)),
        ] {
            assert_eq!(
                query_op(&arkiv, budget).unwrap().budget,
                Some(expected),
                "{budget:?}"
            );
            assert_eq!(
                count_op(&arkiv.filter, budget).unwrap().budget,
                Some(expected),
                "{budget:?}"
            );
        }
    }

    #[test]
    fn an_offset_of_zero_is_no_offset() {
        let op = query_op(&query(vec![vec![u64_is("n", CompareOp::Eq, 1)]]), None).unwrap();
        assert_eq!(op.offset, None);
        assert_eq!(op.limit, Some(10));
        assert_eq!(op.projection, None);
        assert!(!op.with_total);
    }

    #[test]
    fn a_comparison_is_a_range_bounded_on_its_side() {
        for (op, lower, upper) in [
            (CompareOp::Lt, Unbounded, Excluded(7)),
            (CompareOp::Lte, Unbounded, Included(7)),
            (CompareOp::Gt, Excluded(7), Unbounded),
            (CompareOp::Gte, Included(7), Unbounded),
        ] {
            let converted = query_op(&query(vec![vec![u64_is("n", op, 7)]]), None).unwrap();
            let expected = golemdb::Filter::Range {
                field: cell_name("n"),
                bounds: Bounds::new(U64, lower, upper),
            };
            assert_eq!(converted.filter, expected, "{op:?}");
        }
    }

    /// Arkiv's two's complement becomes GolemDB's sign-flipped encoding.
    #[test]
    fn a_signed_value_is_encoded_as_golemdb_encodes_it() {
        let converted =
            query_op(&query(vec![vec![i32_is("level", CompareOp::Eq, -5)]]), None).unwrap();
        let expected = golemdb::Filter::Eq {
            field: cell_name("level"),
            value: EqValue::of(I32, -5),
        };
        assert_eq!(converted.filter, expected);
    }

    #[test]
    fn a_prefix_is_a_text_prefix() {
        let arkiv = query(vec![vec![predicate(
            "name",
            CompareOp::Prefix,
            TypeId::STR,
            b"ab",
        )]]);
        let expected = golemdb::Filter::Prefix {
            field: cell_name("name"),
            prefix: "ab".into(),
        };
        assert_eq!(query_op(&arkiv, None).unwrap().filter, expected);
    }

    #[test]
    fn an_or_of_equalities_on_one_cell_is_in() {
        let arkiv = query(vec![
            vec![u64_is("n", CompareOp::Eq, 3)],
            vec![u64_is("n", CompareOp::Eq, 1)],
            vec![u64_is("n", CompareOp::Eq, 3)],
        ]);
        let expected = golemdb::Filter::In {
            field: cell_name("n"),
            values: InValues::of(U64, [1, 3]),
        };
        assert_eq!(query_op(&arkiv, None).unwrap().filter, expected);
    }

    /// Arkiv's index rules hold even where GolemDB has an index for it.
    #[test]
    fn an_operation_the_type_does_not_index_is_invalid() {
        for predicate in [
            predicate("name", CompareOp::Gt, TypeId::STR, b"a"),
            u64_is("n", CompareOp::Prefix, 1),
            predicate("blob", CompareOp::Eq, TypeId::BYTES, b"x"),
        ] {
            assert_eq!(
                query_op(&query(vec![vec![predicate.clone()]]), None),
                Err(StoreError::InvalidQuery),
                "{predicate:?}"
            );
        }
    }

    #[test]
    fn a_filter_golemdb_cannot_express_is_not_implemented() {
        let blue = predicate("color", CompareOp::Eq, TypeId::STR, b"blue");
        let one = u64_is("n", CompareOp::Eq, 1);
        let not_blue = Predicate {
            negated: true,
            ..blue.clone()
        };
        let cases = [
            ("every record", vec![]),
            ("every record, as an empty group", vec![vec![]]),
            ("and", vec![vec![blue.clone(), one.clone()]]),
            (
                "or across cells",
                vec![vec![blue.clone()], vec![one.clone()]],
            ),
            (
                "or of ranges",
                vec![
                    vec![u64_is("n", CompareOp::Lt, 1)],
                    vec![u64_is("n", CompareOp::Gt, 5)],
                ],
            ),
            ("not", vec![vec![not_blue]]),
            (
                "a prefix inside a character",
                vec![vec![predicate(
                    "name",
                    CompareOp::Prefix,
                    TypeId::STR,
                    &[0xc3],
                )]],
            ),
        ];
        for (case, groups) in cases {
            assert_eq!(
                query_op(&query(groups), None),
                Err(StoreError::NotImplemented),
                "{case}"
            );
        }
    }

    #[test]
    fn a_sort_takes_its_type_from_the_filter() {
        let mut arkiv = query(vec![vec![u64_is("n", CompareOp::Gte, 1)]]);
        arkiv.sort = Some(Sort {
            cell: "n".into(),
            direction: SortDirection::Descending,
        });
        assert_eq!(
            query_op(&arkiv, None).unwrap().sort,
            [golemdb::SortKey::desc("n", CellType::Uint(Width::W8))]
        );

        arkiv.sort = Some(Sort {
            cell: "other".into(),
            direction: SortDirection::Ascending,
        });
        assert_eq!(query_op(&arkiv, None), Err(StoreError::NotImplemented));
    }

    /// A converted filter finds the cells the adapter wrote, and its page and
    /// count read back as Arkiv's. GolemDB does not page yet, so the op runs
    /// without its limit.
    #[test]
    fn a_converted_filter_finds_what_was_written() {
        let dir = TempDir::new().unwrap();
        let config = GolemDbConfig {
            hash_function: HashAlgorithm::Keccak256,
            cell_limits: CellLimits {
                max_cell_name_len: 32,
                max_str_len: 128,
                max_bytes_len: 1024,
            },
            segments: vec![],
        };
        let store = GolemDbStore::open(dir.path(), &config).unwrap();
        let branch = store.begin(None).unwrap();
        for (n, level, name) in [(1, -5, "alpha"), (2, 3, "beta"), (3, 10, "albert")] {
            let cells = vec![
                (
                    "level".into(),
                    Cell::attribute(TypeId::I32, i32::to_be_bytes(level).to_vec()),
                ),
                (
                    "name".into(),
                    Cell::attribute(TypeId::STR, name.as_bytes().to_vec()),
                ),
            ];
            store
                .create(branch, RecordKey([n; 32]), cells, None)
                .unwrap();
        }
        store.commit(branch).unwrap();

        let found = |predicates: Vec<Predicate>| {
            let arkiv = query(predicates.into_iter().map(|p| vec![p]).collect());
            let mut op = query_op(&arkiv, None).unwrap().with_total();
            op.limit = None;
            let page = store.database.query(None, op).unwrap();
            let result = arkiv_query_result(&page).unwrap();
            let count = store.count(None, &arkiv.filter, None).unwrap().into_value();
            assert_eq!(result.total_matched, Some(count));
            let mut keys: Vec<u8> = result
                .records
                .iter()
                .map(|record| record.key.0[0])
                .collect();
            keys.sort_unstable();
            keys
        };
        assert_eq!(found(vec![i32_is("level", CompareOp::Lt, 0)]), [1]);
        assert_eq!(found(vec![i32_is("level", CompareOp::Gte, 3)]), [2, 3]);
        assert_eq!(
            found(vec![
                i32_is("level", CompareOp::Eq, -5),
                i32_is("level", CompareOp::Eq, 10),
            ]),
            [1, 3]
        );
        assert_eq!(
            found(vec![predicate(
                "name",
                CompareOp::Prefix,
                TypeId::STR,
                b"al"
            )]),
            [1, 3]
        );
    }
}
