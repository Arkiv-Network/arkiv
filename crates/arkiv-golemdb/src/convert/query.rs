//! Arkiv's store [`Query`] and [`Filter`] as a GolemDB
//! [`QueryOp`](golemdb::QueryOp) and [`CountOp`](golemdb::CountOp), as far as
//! GolemDB can express them, and GolemDB's page back as a [`QueryResult`].
//!
//! Arkiv's filter, an OR of AND-groups, becomes GolemDB's `Or` of `And`s.
//! What has no counterpart is [`StoreError::NotImplemented`], as the rest of
//! the adapter reports what GolemDB lacks:
//!
//! - a negated predicate: GolemDB has no NOT;
//! - a filter that matches every record, with no group or an empty one:
//!   GolemDB has no filter for that;
//! - a prefix that ends inside a character: GolemDB's prefix is text;
//! - a sort on a cell the filter does not hold to one type: a GolemDB sort
//!   names one.
//!
//! An offset of 0 is no offset, so it is dropped.
//!
//! One difference is not in the query at all: unsorted, and as a sort's
//! tie-break, GolemDB returns records in record ID order and Arkiv in key
//! order.

use std::ops::Bound::{Excluded, Included, Unbounded};

use arkiv_interfaces::store::{
    AndGroup, Budget, Cell, CellKind, CompareOp, Filter, Predicate, Query, QueryResult, Sort,
    SortDirection, StoreError, Test, TypeId,
};
use golemdb_api as golemdb;
use golemdb_api::{Bounds, EqValue, ExistsType, InValues};

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
        .map(|sort| sort_key(sort, &query.filter))
        .transpose()?;
    let mut op = golemdb::QueryOp::from_filter(filter, golemdb_budget(budget))
        .sort(sort)
        .limit(query.page.limit);
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
    Ok(golemdb::CountOp::filter(
        golemdb_filter(filter)?,
        golemdb_budget(budget),
    ))
}

/// Arkiv's budget as GolemDB's. No budget is an unbudgeted call, which GolemDB
/// spells `Unlimited`. A cap carries over as the same number: GolemDB prices
/// no work yet, so its units and Arkiv's cost units are not reconciled.
pub(crate) fn golemdb_budget(budget: Option<Budget>) -> golemdb::Budget {
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

/// Arkiv's DNF as GolemDB's tree: an `Or` of the groups, each an `And` of its
/// predicates, with a node of one child left out. Both evaluate their children
/// in the written order, as Arkiv's groups and predicates are.
///
/// Every predicate is checked against its type's index first, as Arkiv checks
/// it: a range on a `str` is invalid even though GolemDB could run it.
fn golemdb_filter(filter: &Filter) -> Result<golemdb::Filter, StoreError> {
    for predicate in filter.0.iter().flat_map(|group| &group.0) {
        if !predicate.test.is_answerable() {
            return Err(StoreError::InvalidQuery);
        }
    }
    if let Some(any_of) = equalities_on_one_cell(filter)? {
        return Ok(any_of);
    }
    let groups = filter
        .0
        .iter()
        .map(|AndGroup(predicates)| {
            let leaves = predicates.iter().map(leaf).collect::<Result<_, _>>()?;
            node(leaves, golemdb::Filter::And)
        })
        .collect::<Result<_, _>>()?;
    node(groups, golemdb::Filter::Or)
}

/// `children` under `node`, or the one child alone. No child matches every
/// record, an empty group as much as an empty filter, and GolemDB has no
/// filter for that.
fn node(
    mut children: Vec<golemdb::Filter>,
    node: fn(Vec<golemdb::Filter>) -> golemdb::Filter,
) -> Result<golemdb::Filter, StoreError> {
    match children.len() {
        0 => Err(StoreError::NotImplemented),
        1 => Ok(children.remove(0)),
        2.. => Ok(node(children)),
    }
}

/// An OR of equalities on one cell, of one type, is GolemDB's `In`: one leaf
/// rather than an `Or` of several. `None` for any other filter.
fn equalities_on_one_cell(filter: &Filter) -> Result<Option<golemdb::Filter>, StoreError> {
    /// One group's predicate as an equality: its cell, type and operand.
    /// `None` for anything else, including a presence test, which names no
    /// value for `In` to hold.
    fn equality(predicate: &Predicate) -> Option<Equality<'_>> {
        match &predicate.test {
            Test::Compare {
                op: CompareOp::Eq,
                type_id,
                value,
            } if !predicate.negated => Some((&predicate.cell, *type_id, value)),
            Test::Compare { .. } | Test::Exists { .. } => None,
        }
    }

    let equalities: Option<Vec<Equality<'_>>> = filter
        .0
        .iter()
        .map(|AndGroup(predicates)| match predicates.as_slice() {
            [predicate] => equality(predicate),
            [] | [_, _, ..] => None,
        })
        .collect();
    let (&(first_cell, first_type, _), equalities) = match equalities.as_deref() {
        Some(equalities @ [first, _, ..]) => (first, equalities),
        // A group that is not one equality, or fewer than two groups.
        None | Some([] | [_]) => return Ok(None),
    };
    let alike = |(cell, type_id, _): &Equality<'_>| *cell == first_cell && *type_id == first_type;
    if !equalities.iter().all(alike) {
        return Ok(None);
    }
    let values = equalities
        .iter()
        .map(|(_, type_id, value)| operand(*type_id, value))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(golemdb::Filter::In {
        field: cell_name(first_cell),
        values: InValues::try_from(values).map_err(invalid_query)?,
    }))
}

/// An un-negated equality, decomposed: the cell it is on, the type it holds it
/// to, and the operand bytes.
type Equality<'a> = (&'a str, TypeId, &'a [u8]);

/// One predicate as a leaf. GolemDB has no NOT, so a negated predicate has
/// none. `KEY` and `BYTES32` are one GolemDB type, so a leaf of either matches
/// both.
fn leaf(predicate: &Predicate) -> Result<golemdb::Filter, StoreError> {
    if predicate.negated {
        return Err(StoreError::NotImplemented);
    }
    let field = cell_name(&predicate.cell);
    let (op, type_id, value) = match &predicate.test {
        // `exists`/`typeof` are GolemDB's own `Exists` leaf, which takes the
        // type the same way: none for any, one for exactly that.
        Test::Exists { type_id } => {
            let ty = match type_id {
                // Two conversions, each with its own error: Arkiv's type to
                // GolemDB's, then that to an indexable one.
                Some(type_id) => {
                    Some(ExistsType::try_from(cell_type(*type_id)?).map_err(invalid_query)?)
                }
                None => None,
            };
            return Ok(golemdb::Filter::Exists { field, ty });
        }
        Test::Compare { op, type_id, value } => (*op, *type_id, value),
    };
    let bounds =
        |lower, upper| Bounds::try_new(cell_type(type_id)?, lower, upper).map_err(invalid_query);
    Ok(match op {
        CompareOp::Eq => golemdb::Filter::Eq {
            field,
            value: EqValue::try_from(operand(type_id, value)?).map_err(invalid_query)?,
        },
        CompareOp::Lt => golemdb::Filter::Range {
            field,
            bounds: bounds(Unbounded, Excluded(operand(type_id, value)?))?,
        },
        CompareOp::Lte => golemdb::Filter::Range {
            field,
            bounds: bounds(Unbounded, Included(operand(type_id, value)?))?,
        },
        CompareOp::Gt => golemdb::Filter::Range {
            field,
            bounds: bounds(Excluded(operand(type_id, value)?), Unbounded)?,
        },
        CompareOp::Gte => golemdb::Filter::Range {
            field,
            bounds: bounds(Included(operand(type_id, value)?), Unbounded)?,
        },
        CompareOp::Prefix => golemdb::Filter::Prefix {
            field,
            // GolemDB's prefix is text, so a prefix that ends inside a
            // character cannot be asked.
            prefix: String::from_utf8(value.clone()).map_err(|_| StoreError::NotImplemented)?,
        },
    })
}

/// Arkiv sorts by whatever type the cell holds, GolemDB by the cell read as one
/// type. The two agree when the filter holds the cell to one type, since every
/// match then holds that type there. Otherwise there is no type to give.
fn sort_key(sort: &Sort, filter: &Filter) -> Result<golemdb::SortKey, StoreError> {
    let type_id = held_type(filter, &sort.cell).ok_or(StoreError::NotImplemented)?;
    let ty = cell_type(type_id)?;
    Ok(match sort.direction {
        SortDirection::Ascending => golemdb::SortKey::asc(&sort.cell, ty),
        SortDirection::Descending => golemdb::SortKey::desc(&sort.cell, ty),
    })
}

/// The type `filter` holds the cell `name` to, if it holds it to one: a group
/// does when any of its predicates is on that cell, unless negated, and the
/// filter when every group does, to the same type.
fn held_type(filter: &Filter, name: &str) -> Option<TypeId> {
    /// The type a test holds its cell to, if it holds it to one. A bare
    /// `exists` spans every indexed type, so it holds the cell to none.
    const fn type_of(test: &Test) -> Option<TypeId> {
        match test {
            Test::Compare { type_id, .. }
            | Test::Exists {
                type_id: Some(type_id),
            } => Some(*type_id),
            Test::Exists { type_id: None } => None,
        }
    }

    let mut groups = filter.0.iter().map(|AndGroup(predicates)| {
        predicates
            .iter()
            .filter(|predicate| predicate.cell == name && !predicate.negated)
            .find_map(|predicate| type_of(&predicate.test))
    });
    let first = groups.next()??;
    groups
        .all(|type_id| type_id == Some(first))
        .then_some(first)
}

/// A predicate's operand encoded as [`golemdb_cell`] writes a cell, so a leaf
/// finds what was stored. A field, which is what a leaf's values are.
fn operand(type_id: TypeId, value: &[u8]) -> Result<golemdb::CellValue, StoreError> {
    golemdb_cell(&Cell {
        kind: CellKind::Field,
        type_id,
        value: value.to_vec(),
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
    use arkiv_interfaces::store::{Page, RecordKey, Store};
    use golemdb_api::marker::{I32, Str, U64};
    use golemdb_api::{Api, CellLimits, CellType, HashAlgorithm, Width};
    use std::ops::Bound;
    use tempfile::TempDir;

    fn predicate(cell: &str, op: CompareOp, type_id: TypeId, value: &[u8]) -> Predicate {
        Predicate {
            cell: cell.into(),
            test: Test::Compare {
                op,
                type_id,
                value: value.to_vec(),
            },
            negated: false,
        }
    }

    /// A presence predicate: `exists` with no type, `typeof` with one.
    fn presence(cell: &str, type_id: Option<TypeId>) -> Predicate {
        Predicate {
            cell: cell.into(),
            test: Test::Exists { type_id },
            negated: false,
        }
    }

    fn u64_is(cell: &str, op: CompareOp, value: u64) -> Predicate {
        predicate(cell, op, TypeId::U64, &value.to_be_bytes())
    }

    fn i32_is(cell: &str, op: CompareOp, value: i32) -> Predicate {
        predicate(cell, op, TypeId::I32, &value.to_be_bytes())
    }

    fn str_is(cell: &str, op: CompareOp, value: &str) -> Predicate {
        predicate(cell, op, TypeId::STR, value.as_bytes())
    }

    fn str_leaf(cell: &str, value: &str) -> golemdb::Filter {
        golemdb::Filter::Eq {
            field: cell_name(cell),
            value: EqValue::of(Str, value),
        }
    }

    fn u64_leaf(cell: &str, value: u64) -> golemdb::Filter {
        golemdb::Filter::Eq {
            field: cell_name(cell),
            value: EqValue::of(U64, value),
        }
    }

    fn u64_range(cell: &str, lower: Bound<u64>, upper: Bound<u64>) -> golemdb::Filter {
        golemdb::Filter::Range {
            field: cell_name(cell),
            bounds: Bounds::new(U64, lower, upper),
        }
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

        let expected = golemdb::QueryOp::from_filter(
            golemdb::Filter::Eq {
                field: cell_name("color"),
                value: EqValue::of(Str, "blue"),
            },
            golemdb::Budget::Limited(1_000),
        )
        .only(["color", "size"])
        .limit(10)
        .offset(20)
        .with_total();
        assert_eq!(query_op(&arkiv, Some(Budget(1_000))), Ok(expected));
    }

    /// A presence predicate is GolemDB's own `Exists` leaf, which takes the
    /// type the same way: absent for any, present for exactly one.
    #[test]
    fn a_presence_predicate_is_an_exists_leaf() {
        assert_eq!(
            golemdb_filter(&Filter(vec![AndGroup(vec![presence("team", None)])])),
            Ok(golemdb::Filter::Exists {
                field: cell_name("team"),
                ty: None,
            })
        );
        assert_eq!(
            golemdb_filter(&Filter(vec![AndGroup(vec![presence(
                "team",
                Some(TypeId::STR)
            )])])),
            Ok(golemdb::Filter::Exists {
                field: cell_name("team"),
                ty: Some(ExistsType::of(Str)),
            })
        );
    }

    /// GolemDB has no NOT, so a negated presence test has no leaf — as a
    /// negated comparison has none.
    #[test]
    fn a_negated_presence_predicate_is_not_implemented() {
        let mut negated = presence("team", None);
        negated.negated = true;
        assert_eq!(
            golemdb_filter(&Filter(vec![AndGroup(vec![negated])])),
            Err(StoreError::NotImplemented)
        );
    }

    /// A type that is never indexed cannot be asked for, the same way a range
    /// on an equality-only type cannot.
    #[test]
    fn a_presence_predicate_over_a_field_only_type_is_invalid() {
        assert_eq!(
            golemdb_filter(&Filter(vec![AndGroup(vec![presence(
                "payload",
                Some(TypeId::BYTES)
            )])])),
            Err(StoreError::InvalidQuery)
        );
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
                expected,
                "{budget:?}"
            );
            assert_eq!(
                count_op(&arkiv.filter, budget).unwrap().budget,
                expected,
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
    fn a_group_is_an_and_of_its_predicates() {
        let arkiv = query(vec![vec![
            str_is("color", CompareOp::Eq, "blue"),
            u64_is("n", CompareOp::Gte, 2),
        ]]);
        let expected = golemdb::Filter::And(vec![
            str_leaf("color", "blue"),
            u64_range("n", Included(2), Unbounded),
        ]);
        assert_eq!(query_op(&arkiv, None).unwrap().filter, expected);
    }

    /// A group of one predicate is its leaf, not an `And` of one.
    #[test]
    fn groups_are_an_or_of_their_ands() {
        let arkiv = query(vec![
            vec![
                str_is("color", CompareOp::Eq, "blue"),
                u64_is("n", CompareOp::Eq, 1),
            ],
            vec![str_is("color", CompareOp::Eq, "red")],
        ]);
        let expected = golemdb::Filter::Or(vec![
            golemdb::Filter::And(vec![str_leaf("color", "blue"), u64_leaf("n", 1)]),
            str_leaf("color", "red"),
        ]);
        assert_eq!(query_op(&arkiv, None).unwrap().filter, expected);
    }

    /// Only equalities on one cell, of one type, make an `In`.
    #[test]
    fn any_other_or_of_leaves_is_an_or() {
        let cases = [
            (
                "across cells",
                vec![
                    vec![str_is("color", CompareOp::Eq, "blue")],
                    vec![u64_is("n", CompareOp::Eq, 1)],
                ],
                vec![str_leaf("color", "blue"), u64_leaf("n", 1)],
            ),
            (
                "of ranges",
                vec![
                    vec![u64_is("n", CompareOp::Lt, 1)],
                    vec![u64_is("n", CompareOp::Gt, 5)],
                ],
                vec![
                    u64_range("n", Unbounded, Excluded(1)),
                    u64_range("n", Excluded(5), Unbounded),
                ],
            ),
            (
                "across types",
                vec![
                    vec![u64_is("n", CompareOp::Eq, 1)],
                    vec![i32_is("n", CompareOp::Eq, 1)],
                ],
                vec![
                    u64_leaf("n", 1),
                    golemdb::Filter::Eq {
                        field: cell_name("n"),
                        value: EqValue::of(I32, 1),
                    },
                ],
            ),
        ];
        for (case, groups, children) in cases {
            assert_eq!(
                query_op(&query(groups), None).unwrap().filter,
                golemdb::Filter::Or(children),
                "{case}"
            );
        }
    }

    #[test]
    fn a_filter_golemdb_cannot_express_is_not_implemented() {
        let blue = str_is("color", CompareOp::Eq, "blue");
        let not_blue = Predicate {
            negated: true,
            ..blue.clone()
        };
        let cases = [
            ("every record", vec![]),
            ("every record, as an empty group", vec![vec![]]),
            (
                "every record, as an empty group beside another",
                vec![vec![blue.clone()], vec![]],
            ),
            ("not", vec![vec![not_blue.clone()]]),
            ("a not inside an and", vec![vec![blue.clone(), not_blue]]),
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

    /// An `And` holds a cell to the type any child holds it to, an `Or` to the
    /// one every child holds it to.
    #[test]
    fn a_sort_takes_its_type_through_and_and_or() {
        let sorted = |groups: Vec<Vec<Predicate>>| {
            let mut arkiv = query(groups);
            arkiv.sort = Some(Sort {
                cell: "n".into(),
                direction: SortDirection::Ascending,
            });
            query_op(&arkiv, None).map(|op| op.sort)
        };
        let by_n = Ok(vec![golemdb::SortKey::asc("n", CellType::Uint(Width::W8))]);
        let blue = || str_is("color", CompareOp::Eq, "blue");
        assert_eq!(
            sorted(vec![vec![blue(), u64_is("n", CompareOp::Gte, 1)]]),
            by_n
        );
        assert_eq!(
            sorted(vec![
                vec![u64_is("n", CompareOp::Eq, 1)],
                vec![u64_is("n", CompareOp::Gt, 5)],
            ]),
            by_n
        );
        assert_eq!(
            sorted(vec![vec![u64_is("n", CompareOp::Eq, 1)], vec![blue()]]),
            Err(StoreError::NotImplemented),
            "one child leaves `n` out"
        );
        assert_eq!(
            sorted(vec![
                vec![u64_is("n", CompareOp::Eq, 1)],
                vec![i32_is("n", CompareOp::Eq, 1)],
            ]),
            Err(StoreError::NotImplemented),
            "the children hold `n` to two types"
        );
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

        let found = |groups: Vec<Vec<Predicate>>| {
            let arkiv = query(groups);
            let mut op = query_op(&arkiv, None).unwrap().with_total();
            op.limit = None;
            let page = store.database.query(None, op).into_result().unwrap();
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
        let below_zero = || i32_is("level", CompareOp::Lt, 0);
        let at_least_three = || i32_is("level", CompareOp::Gte, 3);
        let al = || str_is("name", CompareOp::Prefix, "al");
        assert_eq!(found(vec![vec![below_zero()]]), [1]);
        assert_eq!(found(vec![vec![at_least_three()]]), [2, 3]);
        assert_eq!(found(vec![vec![al()]]), [1, 3]);
        assert_eq!(
            found(vec![
                vec![i32_is("level", CompareOp::Eq, -5)],
                vec![i32_is("level", CompareOp::Eq, 10)],
            ]),
            [1, 3],
            "in"
        );
        assert_eq!(found(vec![vec![at_least_three(), al()]]), [3], "and");
        assert_eq!(
            found(vec![
                vec![below_zero()],
                vec![str_is("name", CompareOp::Eq, "beta")]
            ]),
            [1, 2],
            "or across cells"
        );
        assert_eq!(
            found(vec![vec![at_least_three(), al()], vec![below_zero()]]),
            [1, 3],
            "or of an and"
        );
    }
}
