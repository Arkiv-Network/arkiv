//! Lower an Arkiv [`Query`] into a GolemDB [`Filter`].
//!
//! This is what lets entities be stored as native GolemDB records: if the query
//! language cannot be expressed as store filters, entities have to keep carrying their
//! own index structures and `arkiv_query` has to keep walking them.
//!
//! # The shape mismatch
//!
//! An Arkiv [`Query`] is an arbitrary boolean tree — `And`, `Or` and `Not` nest
//! freely. A [`Filter`] is an *ordered DNF*: a fixed two-level OR-of-ANDs, with
//! negation available only per-predicate. Lowering is therefore:
//!
//! 1. **Push `Not` down to the leaves** by De Morgan, where it becomes
//!    [`Predicate::negated`]. The semantics line up: a negated predicate matches
//!    records that lack the attribute entirely (`hit != negated` in
//!    `Predicate::matches`), which is the same complement-over-live-entities that
//!    Arkiv's `Not` means.
//! 2. **Distribute `And` over `Or`** to reach DNF.
//!
//! # Why there is a group cap
//!
//! Step 2 is exponential in the worst case: `(a|b) & (c|d) & (e|f)` is three `Or`s
//! and eight groups, and `n` such clauses give `2^n`. A query well inside the
//! language's byte limit can therefore lower into something enormous, so
//! [`MAX_GROUPS`] bounds the output and [`LowerError::TooManyGroups`] reports the
//! refusal. Without it the parser's limits would not actually bound the work.

use alloc::vec;
use alloc::vec::Vec;

use arkiv_interfaces::entity::{AttributeValue, annotations};
use arkiv_interfaces::query::{AnnotKey, BuiltIn, Query};
use arkiv_interfaces::store::{AndGroup, CellName, CompareOp, Filter, Predicate, TypeId};

/// The most AND-groups a lowered filter may contain. See the module docs.
pub const MAX_GROUPS: usize = 256;

/// Why a query could not be lowered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// Disjunctive normal form would exceed [`MAX_GROUPS`] groups.
    TooManyGroups,
    /// The value's type is never indexed, so no predicate can match it. Today that
    /// is only `$payload`, which the language does not expose as a filter key.
    NotIndexable,
}

/// The store cell an Arkiv attribute is stored under.
///
/// Built-ins go to their [`annotations`] byte strings rather than the language's
/// `$name`, because those constants are the on-chain identity of the attribute; the
/// language name is just how it is spelled in a query.
fn cell_name(key: &AnnotKey) -> CellName {
    match key {
        AnnotKey::User(name) => CellName::from(name.as_str()),
        AnnotKey::BuiltIn(built_in) => {
            let bytes: &[u8] = match built_in {
                BuiltIn::Owner => annotations::OWNER,
                BuiltIn::Creator => annotations::CREATOR,
                BuiltIn::Key => annotations::KEY,
                BuiltIn::ExpiresAt => annotations::EXPIRATION,
                BuiltIn::CreatedAt => annotations::CREATED_AT_BLOCK,
                BuiltIn::ContentType => annotations::CONTENT_TYPE,
            };
            // The constants are ASCII literals in this crate's own source; a
            // non-UTF-8 one would be a compile-time typo, not a runtime input.
            CellName::from(core::str::from_utf8(bytes).expect("annotation names are ASCII"))
        }
    }
}

/// The store type id for a value.
const fn type_id(value: &AttributeValue) -> Result<TypeId, LowerError> {
    Ok(match value {
        AttributeValue::Bool(_) => TypeId::BOOL,
        AttributeValue::Int(_) => TypeId::I32,
        AttributeValue::U64(_) => TypeId::U64,
        AttributeValue::U256(_) => TypeId::U256,
        AttributeValue::Decimal(_) => TypeId::DEC,
        AttributeValue::Bytes32(_) => TypeId::BYTES32,
        AttributeValue::Str(_) => TypeId::STR,
        AttributeValue::EthereumAddress(_) => TypeId::ADDR,
        AttributeValue::EntityKey(_) => TypeId::KEY,
        // `bytes` is field-only: it has no index, so a predicate over it can never
        // be answered. The language does not expose `$payload` as a filter key, so
        // this is unreachable today -- but it is a type error, not a panic.
        AttributeValue::Bytes(_) => return Err(LowerError::NotIndexable),
    })
}

/// One comparison, with negation already resolved.
fn predicate(
    key: &AnnotKey,
    value: &AttributeValue,
    op: CompareOp,
    negated: bool,
) -> Result<Predicate, LowerError> {
    Ok(Predicate {
        cell: cell_name(key),
        op,
        type_id: type_id(value)?,
        // `encode`, never `index_bytes`. The store applies its own `order_encoding`
        // when comparing, and `index_bytes` has already sign-biased `Int`/`Decimal`
        // -- passing it would bias twice and invert the ordering for negatives.
        value: value.encode(),
        negated,
    })
}

/// Lower a query to a filter.
///
/// `negate` carries a pending `Not` down the tree; callers start with `false`.
fn lower_inner(query: &Query, negate: bool) -> Result<Vec<AndGroup>, LowerError> {
    let leaf = |key, value, op| -> Result<Vec<AndGroup>, LowerError> {
        Ok(vec![AndGroup(vec![predicate(key, value, op, negate)?])])
    };

    match query {
        // Everything, or under negation nothing. An empty filter matches every
        // record; a filter with no groups matches none.
        Query::All => Ok(if negate {
            Vec::new()
        } else {
            vec![AndGroup(Vec::new())]
        }),

        Query::Eq { key, value } => leaf(key, value, CompareOp::Eq),
        Query::Gt { key, value } => leaf(key, value, CompareOp::Gt),
        Query::Gte { key, value } => leaf(key, value, CompareOp::Gte),
        Query::Lt { key, value } => leaf(key, value, CompareOp::Lt),
        Query::Lte { key, value } => leaf(key, value, CompareOp::Lte),
        Query::StartsWith { key, value } => leaf(key, value, CompareOp::Prefix),

        // De Morgan: under negation, And becomes Or and vice versa.
        Query::And(left, right) | Query::Or(left, right) => {
            let is_and = matches!(query, Query::And(..)) != negate;
            let left = lower_inner(left, negate)?;
            let right = lower_inner(right, negate)?;
            if is_and {
                cross(left, right)
            } else {
                union(left, right)
            }
        }

        Query::Not(inner) => lower_inner(inner, !negate),
    }
}

/// OR: concatenate the groups.
fn union(mut left: Vec<AndGroup>, right: Vec<AndGroup>) -> Result<Vec<AndGroup>, LowerError> {
    if left.len().saturating_add(right.len()) > MAX_GROUPS {
        return Err(LowerError::TooManyGroups);
    }
    left.extend(right);
    Ok(left)
}

/// AND: distribute, which is where DNF can blow up.
fn cross(left: Vec<AndGroup>, right: Vec<AndGroup>) -> Result<Vec<AndGroup>, LowerError> {
    if left.len().saturating_mul(right.len()) > MAX_GROUPS {
        return Err(LowerError::TooManyGroups);
    }
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in &left {
        for r in &right {
            let mut merged = l.0.clone();
            merged.extend(r.0.iter().cloned());
            out.push(AndGroup(merged));
        }
    }
    Ok(out)
}

/// The result of lowering: a filter to run, or the knowledge that nothing can match.
///
/// A [`Filter`] with no groups means *match everything*, so "match nothing" has no
/// representation as one. `NOT(*)` is a legitimate query with exactly that meaning, so
/// it gets its own variant and the caller returns an empty page without touching the
/// store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lowered {
    /// No record can match. Do not query.
    Nothing,
    /// Query with this filter.
    Filter(Filter),
}

/// Lower an Arkiv [`Query`] for the store.
pub fn lower(query: &Query) -> Result<Lowered, LowerError> {
    let groups = lower_inner(query, false)?;
    Ok(if groups.is_empty() {
        Lowered::Nothing
    } else {
        Lowered::Filter(Filter(groups))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use arkiv_interfaces::store::{Cell, Record, RecordKey, RecordVersion};

    fn user(name: &str) -> AnnotKey {
        AnnotKey::User(alloc::string::String::from(name))
    }

    fn eq(name: &str, n: i32) -> Query {
        Query::Eq {
            key: user(name),
            value: AttributeValue::Int(n),
        }
    }

    /// A record with the given `i32` attributes set.
    fn record(attrs: &[(&str, i32)]) -> Record {
        Record {
            key: RecordKey([0u8; 32]),
            version: RecordVersion(1),
            cells: attrs
                .iter()
                .map(|(name, n)| {
                    (
                        CellName::from(*name),
                        Cell::attribute(TypeId::I32, AttributeValue::Int(*n).encode()),
                    )
                })
                .collect(),
        }
    }

    /// Evaluate a lowered query the way the store would.
    fn matches(query: &Query, rec: &Record) -> bool {
        match lower(query).expect("lowers") {
            Lowered::Nothing => false,
            Lowered::Filter(filter) => filter.matches(rec).expect("valid"),
        }
    }

    /// The filter a query lowers to, when it lowers to one.
    fn filter_of(query: &Query) -> Filter {
        match lower(query).expect("lowers") {
            Lowered::Filter(filter) => filter,
            Lowered::Nothing => panic!("expected a filter, got Nothing"),
        }
    }

    #[test]
    fn star_matches_everything_and_its_negation_matches_nothing() {
        let any = record(&[("a", 1)]);
        assert!(matches(&Query::All, &any));
        assert_eq!(
            lower(&Query::Not(Box::new(Query::All))),
            Ok(Lowered::Nothing),
            "an empty Filter means *everything*, so NOT(*) cannot be one"
        );
        assert!(!matches(&Query::Not(Box::new(Query::All)), &any));
    }

    #[test]
    fn operators_map_one_to_one() {
        let rec = record(&[("a", 5)]);
        let at = |q: Query| matches(&q, &rec);
        assert!(at(eq("a", 5)));
        assert!(at(Query::Gt {
            key: user("a"),
            value: AttributeValue::Int(4)
        }));
        assert!(at(Query::Gte {
            key: user("a"),
            value: AttributeValue::Int(5)
        }));
        assert!(at(Query::Lt {
            key: user("a"),
            value: AttributeValue::Int(6)
        }));
        assert!(at(Query::Lte {
            key: user("a"),
            value: AttributeValue::Int(5)
        }));
        assert!(!at(eq("a", 4)));
    }

    #[test]
    fn negative_integers_order_correctly() {
        // The trap this guards: `index_bytes` sign-biases `Int`, and the store's
        // `compare` biases again via `order_encoding`. Lowering with `index_bytes`
        // would double-bias and make -5 compare as a large positive.
        let rec = record(&[("a", -5)]);
        assert!(matches(
            &Query::Lt {
                key: user("a"),
                value: AttributeValue::Int(0)
            },
            &rec
        ));
        assert!(matches(
            &Query::Gt {
                key: user("a"),
                value: AttributeValue::Int(-6)
            },
            &rec
        ));
        assert!(!matches(
            &Query::Gt {
                key: user("a"),
                value: AttributeValue::Int(0)
            },
            &rec
        ));
    }

    #[test]
    fn and_or_nest_into_dnf() {
        // (a=1 OR a=2) AND b=3  ->  two groups, each carrying b=3.
        let query = Query::And(
            Box::new(Query::Or(Box::new(eq("a", 1)), Box::new(eq("a", 2)))),
            Box::new(eq("b", 3)),
        );
        let filter = filter_of(&query);
        assert_eq!(filter.0.len(), 2);
        assert!(filter.0.iter().all(|group| group.0.len() == 2));

        assert!(matches(&query, &record(&[("a", 1), ("b", 3)])));
        assert!(matches(&query, &record(&[("a", 2), ("b", 3)])));
        assert!(
            !matches(&query, &record(&[("a", 3), ("b", 3)])),
            "neither disjunct"
        );
        assert!(
            !matches(&query, &record(&[("a", 1), ("b", 4)])),
            "conjunct fails"
        );
    }

    #[test]
    fn de_morgan_pushes_not_to_the_leaves() {
        // NOT(a=1 AND b=2) must equal (a!=1) OR (b!=2), evaluated over records that
        // may not carry the attribute at all.
        let query = Query::Not(Box::new(Query::And(
            Box::new(eq("a", 1)),
            Box::new(eq("b", 2)),
        )));
        assert!(
            !matches(&query, &record(&[("a", 1), ("b", 2)])),
            "both hold, so NOT is false"
        );
        assert!(matches(&query, &record(&[("a", 1), ("b", 9)])));
        assert!(matches(&query, &record(&[("a", 9), ("b", 2)])));
        assert!(
            matches(&query, &record(&[])),
            "absent attributes are in the complement"
        );
    }

    #[test]
    fn double_negation_cancels() {
        let query = Query::Not(Box::new(Query::Not(Box::new(eq("a", 1)))));
        assert!(matches(&query, &record(&[("a", 1)])));
        assert!(!matches(&query, &record(&[("a", 2)])));
    }

    #[test]
    fn not_of_or_becomes_an_and_group() {
        let query = Query::Not(Box::new(Query::Or(
            Box::new(eq("a", 1)),
            Box::new(eq("b", 2)),
        )));
        let filter = filter_of(&query);
        assert_eq!(filter.0.len(), 1, "NOT(x OR y) is a single conjunction");
        assert_eq!(filter.0[0].0.len(), 2);
        assert!(filter.0[0].0.iter().all(|p| p.negated));
    }

    #[test]
    fn exponential_expansion_is_refused_not_attempted() {
        // Nine OR-clauses AND-ed together is 2^9 = 512 groups, past the cap. The
        // query is short enough to pass every parser limit, so this is the only
        // thing standing between a small input and a huge amount of work.
        let mut query = Query::Or(Box::new(eq("a", 0)), Box::new(eq("b", 0)));
        for i in 1..9 {
            let clause = Query::Or(Box::new(eq("a", i)), Box::new(eq("b", i)));
            query = Query::And(Box::new(query), Box::new(clause));
        }
        assert_eq!(lower(&query), Err(LowerError::TooManyGroups));
    }

    #[test]
    fn a_built_in_lowers_to_its_annotation_constant() {
        let query = Query::Eq {
            key: AnnotKey::BuiltIn(BuiltIn::Owner),
            value: AttributeValue::EthereumAddress([0xab; 20]),
        };
        let filter = filter_of(&query);
        assert_eq!(
            filter.0[0].0[0].cell,
            CellName::from(core::str::from_utf8(annotations::OWNER).unwrap())
        );
        assert_eq!(filter.0[0].0[0].type_id, TypeId::ADDR);
    }

    #[test]
    fn an_unindexable_value_is_rejected() {
        let query = Query::Eq {
            key: user("payload"),
            value: AttributeValue::Bytes(vec![1, 2, 3]),
        };
        assert_eq!(lower(&query), Err(LowerError::NotIndexable));
    }
}
