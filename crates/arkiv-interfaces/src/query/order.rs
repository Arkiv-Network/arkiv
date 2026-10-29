//! An [`OrderBy`] as the store [`Sort`] it runs as: the attribute's cell,
//! read as the type the attribute is stored under.

use super::{Direction, OrderBy};
use crate::entity_records::type_id_for;
use crate::store::{Sort, SortDirection};

impl From<&OrderBy> for Sort {
    fn from(order: &OrderBy) -> Self {
        Self {
            cell: order.key.cell_name(),
            type_id: type_id_for(order.attr_type),
            direction: match order.direction {
                Direction::Ascending => SortDirection::Ascending,
                Direction::Descending => SortDirection::Descending,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::AttributeType;
    use crate::query::{AnnotKey, BuiltIn};
    use crate::store::{CellName, TypeId};
    use alloc::string::String;

    #[test]
    fn a_user_attribute_sorts_its_cell_as_its_type() {
        let order = OrderBy {
            key: AnnotKey::User(String::from("rank")),
            attr_type: AttributeType::U256,
            direction: Direction::Descending,
        };
        assert_eq!(
            Sort::from(&order),
            Sort {
                cell: CellName::from("rank"),
                type_id: TypeId::U256,
                direction: SortDirection::Descending,
            }
        );
    }

    /// A system attribute sorts its `$` cell, under the type the record
    /// mapping writes it as.
    #[test]
    fn a_system_attribute_sorts_its_annotation_cell() {
        let order = OrderBy {
            key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
            attr_type: AttributeType::U64,
            direction: Direction::Ascending,
        };
        assert_eq!(
            Sort::from(&order),
            Sort {
                cell: CellName::from("$expiresAt"),
                type_id: TypeId::U64,
                direction: SortDirection::Ascending,
            }
        );
    }
}
