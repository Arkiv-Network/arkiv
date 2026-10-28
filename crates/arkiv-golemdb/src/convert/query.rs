//! Arkiv's store [`Query`] and [`Filter`] as a GolemDB
//! [`QueryOp`](golemdb::QueryOp) and [`CountOp`](golemdb::CountOp), as far as
//! GolemDB can express them, and GolemDB's page back as a [`QueryResult`].
//!
//! The filter crosses node for node: an `And` or `Or` is GolemDB's own, over
//! the same children in the same order, and each predicate is one leaf. A
//! `Between` is one `Range` leaf, both sides included.
//! Nothing is merged or rearranged, so GolemDB runs the query as it was
//! written. What has no counterpart is [`StoreError::NotImplemented`], as the
//! rest of the adapter reports what GolemDB lacks:
//!
//! - a `Not`: GolemDB has no NOT;
//! - [`Filter::All`], and an `And` or `Or` with no child: GolemDB has no filter
//!   for every record, or for none;
//! - a presence test for `key`: GolemDB stores a `key` as the 32 fixed bytes
//!   a `bytes32` is, so it cannot tell the two apart;
//! - a sort on a cell the filter does not hold to one type: a GolemDB sort
//!   names one.
//!
//! A query keeps well within GolemDB's cap of 64 nested `And`/`Or` levels: the
//! parser builds a chain of one connective as one node and refuses a query
//! nesting `AND`/`OR` more than four levels, and the RPC's live-entity bound
//! adds one.

use std::ops::Bound::{Excluded, Included, Unbounded};

use arkiv_interfaces::store::{
    Budget, Cell, CellKind, CompareOp, Filter, Predicate, Query, QueryResult, Sort, SortDirection,
    StoreError, Test, TypeId,
};
use golemdb_api as golemdb;
use golemdb_api::{Bounds, EqValue, ExistsType};

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

/// The filter as GolemDB's, node for node. Both evaluate children in the
/// written order.
///
/// Every predicate in the tree is checked against its type's index before conversion.
fn golemdb_filter(filter: &Filter) -> Result<golemdb::Filter, StoreError> {
    if !answerable(filter) {
        return Err(StoreError::InvalidQuery);
    }
    node(filter)
}

/// Whether every predicate in the tree can be answered at all.
fn answerable(filter: &Filter) -> bool {
    match filter {
        Filter::All => true,
        Filter::Predicate(predicate) => predicate.test.is_answerable(),
        Filter::And(children) | Filter::Or(children) => children.iter().all(answerable),
        Filter::Not(child) => answerable(child),
    }
}

/// One node as GolemDB's.
fn node(filter: &Filter) -> Result<golemdb::Filter, StoreError> {
    match filter {
        Filter::Predicate(predicate) => leaf(predicate),
        Filter::And(children) => children_of(children).map(golemdb::Filter::And),
        Filter::Or(children) => children_of(children).map(golemdb::Filter::Or),
        // GolemDB has no NOT, and no filter for every record.
        Filter::Not(_) | Filter::All => Err(StoreError::NotImplemented),
    }
}

/// A node's children, in order. GolemDB's `And` and `Or` need at least one:
/// with none, an `And` is every record and an `Or` none, which GolemDB has no
/// filter for.
fn children_of(children: &[Filter]) -> Result<Vec<golemdb::Filter>, StoreError> {
    if children.is_empty() {
        return Err(StoreError::NotImplemented);
    }
    children.iter().map(node).collect()
}

/// One predicate as a leaf. `KEY` and `BYTES32` are one GolemDB type, so a
/// comparison leaf of either matches both, and a presence test for `KEY` has
/// none.
fn leaf(predicate: &Predicate) -> Result<golemdb::Filter, StoreError> {
    let field = cell_name(&predicate.cell);
    let (op, type_id, value) = match &predicate.test {
        // `exists`/`typeof` are GolemDB's own `Exists` leaf, which takes the
        // type the same way: none for any, one for exactly that.
        Test::Exists { type_id } => {
            let ty = match type_id {
                // `typeof(attr) = key` would also answer every `bytes32`
                // holder. A `key` attribute reads back from GolemDB as a
                // `bytes32` anyway, so `typeof(attr) = bytes32` is the one
                // GolemDB answers, and it finds both.
                Some(TypeId::KEY) => return Err(StoreError::NotImplemented),
                // Two conversions, each with its own error: Arkiv's type to
                // GolemDB's, then that to an indexable one.
                Some(type_id) => {
                    Some(ExistsType::try_from(cell_type(*type_id)?).map_err(invalid_query)?)
                }
                None => None,
            };
            return Ok(golemdb::Filter::Exists { field, ty });
        }
        // `BETWEEN` is GolemDB's range with both sides included, which takes
        // inverted bounds as matching nothing, as the language does.
        Test::Between { type_id, low, high } => {
            let bounds = Bounds::try_new(
                cell_type(*type_id)?,
                Included(operand(*type_id, low)?),
                Included(operand(*type_id, high)?),
            )
            .map_err(invalid_query)?;
            return Ok(golemdb::Filter::Range { field, bounds });
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

/// The type `filter` holds the cell `name` to, if it holds it to one: a
/// predicate on that cell holds it to its type, an `And` to the type any child
/// holds it to, and an `Or` to the one every child holds it to. A `Not` holds
/// it to none, and so does a bare `exists`, which spans every indexed type.
fn held_type(filter: &Filter, name: &str) -> Option<TypeId> {
    match filter {
        Filter::Predicate(Predicate { cell, test }) if cell == name => match test {
            Test::Compare { type_id, .. }
            | Test::Between { type_id, .. }
            | Test::Exists {
                type_id: Some(type_id),
            } => Some(*type_id),
            Test::Exists { type_id: None } => None,
        },
        Filter::Predicate(_) | Filter::All | Filter::Not(_) => None,
        Filter::And(children) => children.iter().find_map(|child| held_type(child, name)),
        Filter::Or(children) => {
            let mut types = children.iter().map(|child| held_type(child, name));
            let first = types.next()??;
            types.all(|type_id| type_id == Some(first)).then_some(first)
        }
    }
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
    use arkiv_interfaces::entity_records::{MAX_RECORD_CELLS, MAX_RECORD_INDEXED_CELLS};
    use arkiv_interfaces::store::{Page, RecordKey, Store};
    use golemdb_api::marker::{Bool, Bytes20, Bytes32, I32, Str, U64};
    use golemdb_api::{Api, CellLimits, CellType, Genesis, HashAlgorithm, RecordLimits, Width};
    use std::ops::Bound;
    use tempfile::TempDir;

    fn predicate(cell: &str, op: CompareOp, type_id: TypeId, value: &[u8]) -> Filter {
        Filter::Predicate(Predicate {
            cell: cell.into(),
            test: Test::Compare {
                op,
                type_id,
                value: value.to_vec(),
            },
        })
    }

    fn between(cell: &str, type_id: TypeId, low: &[u8], high: &[u8]) -> Filter {
        Filter::Predicate(Predicate {
            cell: cell.into(),
            test: Test::Between {
                type_id,
                low: low.to_vec(),
                high: high.to_vec(),
            },
        })
    }

    /// A presence predicate: `exists` with no type, `typeof` with one.
    fn presence(cell: &str, type_id: Option<TypeId>) -> Filter {
        Filter::Predicate(Predicate {
            cell: cell.into(),
            test: Test::Exists { type_id },
        })
    }

    fn u64_is(cell: &str, op: CompareOp, value: u64) -> Filter {
        predicate(cell, op, TypeId::U64, &value.to_be_bytes())
    }

    fn i32_is(cell: &str, op: CompareOp, value: i32) -> Filter {
        predicate(cell, op, TypeId::I32, &value.to_be_bytes())
    }

    fn str_is(cell: &str, op: CompareOp, value: &str) -> Filter {
        predicate(cell, op, TypeId::STR, value.as_bytes())
    }

    fn not(filter: Filter) -> Filter {
        Filter::Not(Box::new(filter))
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

    fn query(filter: Filter) -> Query {
        Query {
            filter,
            page: Page {
                offset: 0,
                limit: 10,
            },
            ..Query::default()
        }
    }

    #[test]
    fn one_equality_maps_every_field() {
        let mut arkiv = query(predicate("color", CompareOp::Eq, TypeId::STR, b"blue"));
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

    /// The tree crosses as it is: the same nodes, nested the same way, over
    /// the same children in the same order.
    #[test]
    fn the_tree_crosses_node_for_node() {
        let arkiv = Filter::Or(vec![
            Filter::And(vec![
                str_is("color", CompareOp::Eq, "blue"),
                u64_is("n", CompareOp::Gte, 2),
            ]),
            str_is("color", CompareOp::Eq, "red"),
        ]);
        assert_eq!(
            golemdb_filter(&arkiv),
            Ok(golemdb::Filter::Or(vec![
                golemdb::Filter::And(vec![
                    str_leaf("color", "blue"),
                    u64_range("n", Included(2), Unbounded),
                ]),
                str_leaf("color", "red"),
            ]))
        );
    }

    /// Nothing is merged or collapsed: an `Or` of equalities on one cell stays
    /// an `Or`, not GolemDB's `In`, and a node of one child stays a node.
    #[test]
    fn nothing_is_merged_or_collapsed() {
        assert_eq!(
            golemdb_filter(&Filter::Or(vec![
                u64_is("n", CompareOp::Eq, 3),
                u64_is("n", CompareOp::Eq, 1),
            ])),
            Ok(golemdb::Filter::Or(vec![
                u64_leaf("n", 3),
                u64_leaf("n", 1)
            ]))
        );
        assert_eq!(
            golemdb_filter(&Filter::And(vec![u64_is("n", CompareOp::Eq, 3)])),
            Ok(golemdb::Filter::And(vec![u64_leaf("n", 3)]))
        );
    }

    /// A presence predicate is GolemDB's own `Exists` leaf, which takes the
    /// type the same way: absent for any, present for exactly one.
    #[test]
    fn a_presence_predicate_is_an_exists_leaf() {
        assert_eq!(
            golemdb_filter(&presence("team", None)),
            Ok(golemdb::Filter::Exists {
                field: cell_name("team"),
                ty: None,
            })
        );
        assert_eq!(
            golemdb_filter(&presence("team", Some(TypeId::STR))),
            Ok(golemdb::Filter::Exists {
                field: cell_name("team"),
                ty: Some(ExistsType::of(Str)),
            })
        );
    }

    /// A type that is never indexed cannot be asked for, the same way a prefix
    /// on a type without one cannot.
    #[test]
    fn a_presence_predicate_over_a_field_only_type_is_invalid() {
        assert_eq!(
            golemdb_filter(&presence("payload", Some(TypeId::BYTES))),
            Err(StoreError::InvalidQuery)
        );
    }

    /// GolemDB stores a `key` as the 32 fixed bytes a `bytes32` is, so a
    /// presence test for `key` has no leaf, while one for `bytes32` is the
    /// `Exists` leaf of those bytes, which a `key` holder matches too.
    #[test]
    fn a_presence_predicate_for_key_is_not_implemented() {
        assert_eq!(
            golemdb_filter(&presence("parent", Some(TypeId::KEY))),
            Err(StoreError::NotImplemented)
        );
        assert_eq!(
            golemdb_filter(&presence("parent", Some(TypeId::BYTES32))),
            Ok(golemdb::Filter::Exists {
                field: cell_name("parent"),
                ty: Some(ExistsType::of(Bytes32)),
            })
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
        let arkiv = query(u64_is("n", CompareOp::Eq, 1));
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
        let op = query_op(&query(u64_is("n", CompareOp::Eq, 1)), None).unwrap();
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
            let converted = query_op(&query(u64_is("n", op, 7)), None).unwrap();
            assert_eq!(converted.filter, u64_range("n", lower, upper), "{op:?}");
        }
    }

    /// A `Between` is one range, not an `And` of two, with both sides
    /// included and a signed value encoded as GolemDB encodes it.
    #[test]
    fn a_between_is_one_range_with_both_sides_included() {
        let converted = query_op(
            &query(between(
                "n",
                TypeId::U64,
                &3u64.to_be_bytes(),
                &7u64.to_be_bytes(),
            )),
            None,
        )
        .unwrap();
        assert_eq!(converted.filter, u64_range("n", Included(3), Included(7)));

        let converted = query_op(
            &query(between(
                "level",
                TypeId::I32,
                &(-5i32).to_be_bytes(),
                &5i32.to_be_bytes(),
            )),
            None,
        )
        .unwrap();
        let expected = golemdb::Filter::Range {
            field: cell_name("level"),
            bounds: Bounds::new(I32, Included(-5), Included(5)),
        };
        assert_eq!(converted.filter, expected);
    }

    /// Every indexed type is ordered, the non-numeric ones included: each
    /// range is GolemDB's own, of that type.
    #[test]
    fn a_range_on_a_non_numeric_type_is_a_range_of_that_type() {
        let at_least = |bounds| golemdb::Filter::Range {
            field: cell_name("a"),
            bounds,
        };
        for (arkiv, expected) in [
            (
                predicate("a", CompareOp::Gte, TypeId::BOOL, &[1]),
                at_least(Bounds::new(Bool, Included(true), Unbounded)),
            ),
            (
                predicate("a", CompareOp::Gte, TypeId::STR, b"blue"),
                at_least(Bounds::new(Str, Included("blue"), Unbounded)),
            ),
            (
                predicate("a", CompareOp::Gte, TypeId::ADDR, &[0xab; 20]),
                at_least(Bounds::new(Bytes20, Included([0xab; 20]), Unbounded)),
            ),
            (
                predicate("a", CompareOp::Gte, TypeId::BYTES32, &[0xab; 32]),
                at_least(Bounds::new(Bytes32, Included([0xab; 32]), Unbounded)),
            ),
            // A `key` is GolemDB's 32 fixed bytes, as a `bytes32` is.
            (
                predicate("a", CompareOp::Gte, TypeId::KEY, &[0xab; 32]),
                at_least(Bounds::new(Bytes32, Included([0xab; 32]), Unbounded)),
            ),
        ] {
            assert_eq!(
                query_op(&query(arkiv.clone()), None).unwrap().filter,
                expected,
                "{arkiv:?}"
            );
        }
    }

    /// Arkiv's two's complement becomes GolemDB's sign-flipped encoding.
    #[test]
    fn a_signed_value_is_encoded_as_golemdb_encodes_it() {
        let converted = query_op(&query(i32_is("level", CompareOp::Eq, -5)), None).unwrap();
        let expected = golemdb::Filter::Eq {
            field: cell_name("level"),
            value: EqValue::of(I32, -5),
        };
        assert_eq!(converted.filter, expected);
    }

    #[test]
    fn a_prefix_is_a_text_prefix() {
        let arkiv = query(predicate("name", CompareOp::Prefix, TypeId::STR, b"ab"));
        let expected = golemdb::Filter::Prefix {
            field: cell_name("name"),
            prefix: "ab".into(),
        };
        assert_eq!(query_op(&arkiv, None).unwrap().filter, expected);
    }

    /// Arkiv's index rules hold anywhere in the tree: under a `Not`, the query
    /// is invalid before it is one GolemDB cannot run.
    #[test]
    fn an_operation_the_type_does_not_index_is_invalid() {
        for invalid in [
            predicate("hash", CompareOp::Prefix, TypeId::BYTES32, &[0; 32]),
            u64_is("n", CompareOp::Prefix, 1),
            predicate("blob", CompareOp::Eq, TypeId::BYTES, b"x"),
            between("blob", TypeId::BYTES, b"x", b"y"),
        ] {
            for filter in [
                invalid.clone(),
                Filter::Or(vec![u64_is("n", CompareOp::Eq, 1), invalid.clone()]),
                not(invalid.clone()),
            ] {
                assert_eq!(
                    query_op(&query(filter.clone()), None),
                    Err(StoreError::InvalidQuery),
                    "{filter:?}"
                );
            }
        }
    }

    #[test]
    fn a_filter_golemdb_cannot_express_is_not_implemented() {
        let blue = || str_is("color", CompareOp::Eq, "blue");
        let cases = [
            ("every record", Filter::All),
            ("every record, as an empty and", Filter::And(vec![])),
            ("no record, as an empty or", Filter::Or(vec![])),
            (
                "an empty and beside another child",
                Filter::Or(vec![blue(), Filter::And(vec![])]),
            ),
            ("not", not(blue())),
            (
                "a not inside an and",
                Filter::And(vec![blue(), not(blue())]),
            ),
            (
                "a prefix inside a character",
                predicate("name", CompareOp::Prefix, TypeId::STR, &[0xc3]),
            ),
        ];
        for (case, filter) in cases {
            assert_eq!(
                query_op(&query(filter), None),
                Err(StoreError::NotImplemented),
                "{case}"
            );
        }
    }

    #[test]
    fn a_sort_takes_its_type_from_the_filter() {
        let mut arkiv = query(u64_is("n", CompareOp::Gte, 1));
        arkiv.sort = Some(Sort {
            cell: "n".into(),
            direction: SortDirection::Descending,
        });
        assert_eq!(
            query_op(&arkiv, None).unwrap().sort,
            [golemdb::SortKey::desc("n", CellType::Uint(Width::W8))]
        );
        // A `Between` holds its cell to its type as a comparison does.
        arkiv.filter = between("n", TypeId::U64, &[0; 8], &[0xff; 8]);
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
        let sorted = |filter: Filter| {
            let mut arkiv = query(filter);
            arkiv.sort = Some(Sort {
                cell: "n".into(),
                direction: SortDirection::Ascending,
            });
            query_op(&arkiv, None).map(|op| op.sort)
        };
        let by_n = Ok(vec![golemdb::SortKey::asc("n", CellType::Uint(Width::W8))]);
        let blue = || str_is("color", CompareOp::Eq, "blue");
        assert_eq!(
            sorted(Filter::And(vec![blue(), u64_is("n", CompareOp::Gte, 1)])),
            by_n
        );
        assert_eq!(
            sorted(Filter::Or(vec![
                u64_is("n", CompareOp::Eq, 1),
                u64_is("n", CompareOp::Gt, 5),
            ])),
            by_n
        );
        assert_eq!(
            sorted(Filter::Or(vec![u64_is("n", CompareOp::Eq, 1), blue()])),
            Err(StoreError::NotImplemented),
            "one child leaves `n` out"
        );
        assert_eq!(
            sorted(Filter::Or(vec![
                u64_is("n", CompareOp::Eq, 1),
                i32_is("n", CompareOp::Eq, 1),
            ])),
            Err(StoreError::NotImplemented),
            "the children hold `n` to two types"
        );
    }

    /// A converted filter finds the cells the adapter wrote, and its page and
    /// count read back as Arkiv's. The op runs without its limit, so the page
    /// holds every match.
    #[test]
    fn a_converted_filter_finds_what_was_written() {
        let dir = TempDir::new().unwrap();
        let limits = CellLimits {
            max_cell_name_len: 32,
            max_str_len: 128,
            max_bytes_len: 1024,
        };
        let record_limits = RecordLimits {
            max_cells: MAX_RECORD_CELLS as u64,
            max_indexed_cells: MAX_RECORD_INDEXED_CELLS as u64,
        };
        let config = GolemDbConfig {
            genesis: Genesis::new(
                HashAlgorithm::Keccak256,
                limits,
                record_limits,
                None,
                golemdb_api::PricingConfig::zero(),
            ),
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

        let found = |filter: Filter| {
            let arkiv = query(filter);
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
        assert_eq!(found(below_zero()), [1]);
        assert_eq!(found(at_least_three()), [2, 3]);
        assert_eq!(found(al()), [1, 3]);
        assert_eq!(
            found(Filter::Or(vec![
                i32_is("level", CompareOp::Eq, -5),
                i32_is("level", CompareOp::Eq, 10),
            ])),
            [1, 3],
            "or on one cell"
        );
        assert_eq!(found(Filter::And(vec![at_least_three(), al()])), [3], "and");
        assert_eq!(
            found(Filter::Or(vec![
                below_zero(),
                str_is("name", CompareOp::Eq, "beta")
            ])),
            [1, 2],
            "or across cells"
        );
        assert_eq!(
            found(Filter::Or(vec![
                Filter::And(vec![at_least_three(), al()]),
                below_zero(),
            ])),
            [1, 3],
            "or of an and"
        );
        assert_eq!(
            found(Filter::And(vec![
                Filter::Or(vec![below_zero(), at_least_three()]),
                al(),
            ])),
            [1, 3],
            "and of an or, as written rather than distributed"
        );
    }
}
