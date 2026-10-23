//! A [`Query`] as the store [`Filter`] it runs as, node for node.
//!
//! Nothing is rearranged: an `And`, `Or` or `Not` becomes the same node over
//! the same children, in the same order, so the store answers the query
//! exactly as it was written. A store that cannot express a node, as GolemDB
//! cannot `Not`, refuses the query rather than being handed an equivalent it
//! can run. Each leaf becomes one [`Predicate`] on its attribute's cell.

use alloc::boxed::Box;

use super::{AnnotKey, AnnotVal, BuiltIn, Query};
use crate::entity::annotations;
use crate::entity_records::{system, type_id_for, type_id_of};
use crate::store::{CellName, CompareOp, Filter, Predicate, Test};

impl AnnotKey {
    /// The store cell this attribute is stored under: a user attribute under
    /// its own name, a built-in under its [`annotations`] constant, the one
    /// definition of each system name, which the record mapping writes under
    /// too.
    pub fn cell_name(&self) -> CellName {
        match self {
            Self::User(name) => name.clone(),
            Self::BuiltIn(built_in) => system(match built_in {
                BuiltIn::Owner => annotations::OWNER,
                BuiltIn::Creator => annotations::CREATOR,
                BuiltIn::Key => annotations::KEY,
                BuiltIn::ExpiresAt => annotations::EXPIRES_AT,
            }),
        }
    }
}

/// See the module docs. `*` is every live *entity*, and a store holds account
/// records beside entity records, so it is not [`Filter::All`]: it is the
/// presence of `$key`, the cell every entity record carries.
impl From<&Query> for Filter {
    fn from(query: &Query) -> Self {
        let compare = |key: &AnnotKey, op, value: &AnnotVal| {
            Self::Predicate(Predicate {
                cell: key.cell_name(),
                test: Test::Compare {
                    op,
                    type_id: type_id_of(value),
                    // `encode`, never `index_bytes`. The store applies its own
                    // `order_encoding` when comparing, and `index_bytes` has
                    // already sign-biased `Int`/`Decimal` -- passing it would
                    // bias twice and invert the ordering for negatives.
                    value: value.encode(),
                },
            })
        };
        match query {
            Query::All => Self::Predicate(Predicate {
                cell: AnnotKey::BuiltIn(BuiltIn::Key).cell_name(),
                test: Test::Exists { type_id: None },
            }),
            Query::Eq { key, value } => compare(key, CompareOp::Eq, value),
            Query::Gt { key, value } => compare(key, CompareOp::Gt, value),
            Query::Gte { key, value } => compare(key, CompareOp::Gte, value),
            Query::Lt { key, value } => compare(key, CompareOp::Lt, value),
            Query::Lte { key, value } => compare(key, CompareOp::Lte, value),
            Query::StartsWith { key, value } => compare(key, CompareOp::Prefix, value),
            Query::Exists { name, attr_type } => Self::Predicate(Predicate {
                cell: name.clone(),
                test: Test::Exists {
                    type_id: attr_type.map(type_id_for),
                },
            }),
            Query::And(children) => Self::And(children.iter().map(Self::from).collect()),
            Query::Or(children) => Self::Or(children.iter().map(Self::from).collect()),
            Query::Not(inner) => Self::Not(Box::new(Self::from(&**inner))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{AttributeType, AttributeValue};
    use crate::store::{Cell, Record, RecordKey, RecordVersion, StoreError, TypeId};
    use alloc::string::String;
    use alloc::vec;

    fn user(name: &str) -> AnnotKey {
        AnnotKey::User(String::from(name))
    }

    fn eq(name: &str, n: i32) -> Query {
        Query::Eq {
            key: user(name),
            value: AttributeValue::Int(n),
        }
    }

    /// The predicate `name = i32(n)`, as the store sees it.
    fn int_eq(name: &str, n: i32) -> Filter {
        Filter::Predicate(Predicate {
            cell: CellName::from(name),
            test: Test::Compare {
                op: CompareOp::Eq,
                type_id: TypeId::I32,
                value: AttributeValue::Int(n).encode(),
            },
        })
    }

    /// An entity record with the given `i32` attributes set.
    fn record(attrs: &[(&str, i32)]) -> Record {
        let key = (
            AnnotKey::BuiltIn(BuiltIn::Key).cell_name(),
            Cell::attribute(TypeId::KEY, [0u8; 32].to_vec()),
        );
        Record {
            key: RecordKey([0u8; 32]),
            version: RecordVersion(1),
            cells: core::iter::once(key)
                .chain(attrs.iter().map(|(name, n)| {
                    (
                        CellName::from(*name),
                        Cell::attribute(TypeId::I32, AttributeValue::Int(*n).encode()),
                    )
                }))
                .collect(),
        }
    }

    /// Evaluate a query the way the store would.
    fn matches(query: &Query, record: &Record) -> bool {
        Filter::from(query).matches(record).expect("valid")
    }

    #[test]
    fn the_filter_has_the_shape_of_the_query() {
        // NOT (a = 1 OR b = 2) AND c = 3 AND d = 4: no De Morgan, no
        // distribution, and the chain stays one node of three.
        let query = Query::And(vec![
            Query::Not(Box::new(Query::Or(vec![eq("a", 1), eq("b", 2)]))),
            eq("c", 3),
            eq("d", 4),
        ]);
        assert_eq!(
            Filter::from(&query),
            Filter::And(vec![
                Filter::Not(Box::new(Filter::Or(vec![int_eq("a", 1), int_eq("b", 2)]))),
                int_eq("c", 3),
                int_eq("d", 4),
            ])
        );
    }

    #[test]
    fn a_double_negation_stays_as_written_and_still_cancels() {
        let query = Query::Not(Box::new(Query::Not(Box::new(eq("a", 1)))));
        assert_eq!(
            Filter::from(&query),
            Filter::Not(Box::new(Filter::Not(Box::new(int_eq("a", 1)))))
        );
        assert!(matches(&query, &record(&[("a", 1)])));
        assert!(!matches(&query, &record(&[("a", 2)])));
    }

    #[test]
    fn star_is_every_entity_record_and_its_negation_none() {
        assert_eq!(
            Filter::from(&Query::All),
            Filter::Predicate(Predicate {
                cell: CellName::from("$key"),
                test: Test::Exists { type_id: None },
            })
        );
        let entity = record(&[("a", 1)]);
        assert!(matches(&Query::All, &entity));
        assert!(!matches(&Query::Not(Box::new(Query::All)), &entity));

        // An account record carries no `$key`, so `*` does not take it.
        let account = Record {
            cells: vec![(
                CellName::from("$balance"),
                Cell::field(TypeId::U256, [0u8; 32].to_vec()),
            )],
            ..entity
        };
        assert!(!matches(&Query::All, &account));
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
        // `compare` biases again via `order_encoding`. Converting with
        // `index_bytes` would double-bias and make -5 compare as a large positive.
        let rec = record(&[("a", -5)]);
        let less_than = |n| Query::Lt {
            key: user("a"),
            value: AttributeValue::Int(n),
        };
        let greater_than = |n| Query::Gt {
            key: user("a"),
            value: AttributeValue::Int(n),
        };
        assert!(matches(&less_than(0), &rec));
        assert!(matches(&greater_than(-6), &rec));
        assert!(!matches(&greater_than(0), &rec));
    }

    #[test]
    fn not_is_the_full_complement() {
        // NOT (a = 1 AND b = 2), over records that may not carry the attribute.
        let query = Query::Not(Box::new(Query::And(vec![eq("a", 1), eq("b", 2)])));
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
    fn a_built_in_is_its_annotation_cell() {
        let query = Query::Eq {
            key: AnnotKey::BuiltIn(BuiltIn::Owner),
            value: AttributeValue::EthereumAddress([0xab; 20]),
        };
        assert_eq!(
            Filter::from(&query),
            Filter::Predicate(Predicate {
                cell: CellName::from("$owner"),
                test: Test::Compare {
                    op: CompareOp::Eq,
                    type_id: TypeId::ADDR,
                    value: AttributeValue::EthereumAddress([0xab; 20]).encode(),
                },
            })
        );
    }

    #[test]
    fn presence_names_its_store_type() {
        let exists = |attr_type| {
            Filter::from(&Query::Exists {
                name: String::from("a"),
                attr_type,
            })
        };
        let presence = |type_id| {
            Filter::Predicate(Predicate {
                cell: CellName::from("a"),
                test: Test::Exists { type_id },
            })
        };
        assert_eq!(exists(None), presence(None));
        assert_eq!(
            exists(Some(AttributeType::U64)),
            presence(Some(TypeId::U64))
        );
    }

    /// The conversion maps every value, and the store refuses the one type it
    /// never indexes.
    #[test]
    fn an_unindexable_value_is_refused_by_the_store() {
        let query = Query::Eq {
            key: user("payload"),
            value: AttributeValue::Bytes(vec![1, 2, 3]),
        };
        assert_eq!(
            Filter::from(&query).matches(&record(&[])),
            Err(StoreError::InvalidQuery)
        );
    }
}
