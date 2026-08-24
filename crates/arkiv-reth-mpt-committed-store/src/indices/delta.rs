//! The index's delta vocabulary: what
//! [`annotation_delta`](crate::indices::annotation::annotation_delta) derives
//! and [`RethAuxStore::apply_delta`](crate::RethAuxStore) folds in.

use arkiv_interfaces::entity::AttributeValue;
use arkiv_interfaces::primitives::EntityAddress;

/// One `(attribute, value)` pair in the index. The value stays typed because
/// the type picks the bucket and the ordering; the bytes actually keyed on are
/// [`AttributeValue::index_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrEntry {
    pub attr: Vec<u8>,
    pub value: AttributeValue,
}

impl AttrEntry {
    pub fn new(attr: impl Into<Vec<u8>>, value: AttributeValue) -> Self {
        Self {
            attr: attr.into(),
            value,
        }
    }
}

/// One entity's index changes. The compact `u64` id the bitmaps are keyed on is
/// the store's concern, never the producer's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuxiliaryEntityDelta {
    pub entity_key: EntityAddress,
    pub inserts: Vec<AttrEntry>,
    pub removes: Vec<AttrEntry>,
}
