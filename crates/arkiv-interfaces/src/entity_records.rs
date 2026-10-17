//! An [`Entity`] as a GolemDB [`Record`], and back.
//!
//! # Why this is here and not in a backend crate
//!
//! These bytes feed the store's `branch_digest`, and that digest is the block's state
//! root. Two nodes that lay an entity out differently compute different roots and
//! split the chain — so the mapping is part of the specification, and lives beside
//! [`order_encoding`](crate::store::order_encoding) and
//! [`Predicate::matches`](crate::store::Predicate::matches) rather than in whichever
//! crate happens to talk to the store.
//!
//! # The layout
//!
//! One entity is one record, keyed by the entity address.
//!
//! | cell                    | kind      | type    | from                       |
//! |-------------------------|-----------|---------|----------------------------|
//! | `$key`                  | attribute | `KEY`   | `key`                      |
//! | `$creator`              | attribute | `ADDR`  | `creator`                  |
//! | `$owner`                | attribute | `ADDR`  | `owner`                    |
//! | `$createdAtBlock`       | attribute | `U64`   | `created_at_block`         |
//! | `$expiration`           | attribute | `U64`   | `expires_at`               |
//! | `$contentType`          | attribute | `STR`   | `content_type`             |
//! | `$payload`              | **field** | `BYTES` | `payload`                  |
//! | [`CELL_LAST_MODIFIED`]  | **field** | `U64`   | `last_modified_at_block`   |
//! | [`CELL_CREATION_FLAGS`] | **field** | `BOOL`* | `creation_flags`           |
//! | *user attribute name*   | attribute | per value | `attributes`             |
//!
//! Attributes are indexed and queryable; fields are not. The split follows the
//! language exactly: the six [`BuiltIn`](crate::query::BuiltIn) keys are attributes,
//! and everything a query cannot name is a field, so nothing is paying for an index
//! it cannot use.
//!
//! \* `creation_flags` is a byte, and `BOOL` is the only single-byte core type. It is
//! a field, so it is never compared or indexed and the tag is only a width marker.
//!
//! # Two numbering schemes that must not be confused
//!
//! [`AttributeType`] and [`TypeId`] both number types, and they **disagree** —
//! `AttributeType::U64` is 3 where [`TypeId::U64`] is 8. Conversion goes through
//! [`type_id_of`], which matches on the value, never on a discriminant.

use alloc::string::String;
use alloc::vec::Vec;

use crate::entity::{Attribute, AttributeValue, CreationFlags, Entity, annotations};
use crate::primitives::EntityAddress;
use crate::store::{Cell, CellKind, CellName, Record, RecordKey, TypeId};

/// The block of the entity's last change. A field: the language cannot query it.
pub const CELL_LAST_MODIFIED: &str = "$lastModifiedAtBlock";
/// The entity's creation flags. A field, for the same reason.
pub const CELL_CREATION_FLAGS: &str = "$creationFlags";

/// Why an entity could not be mapped, in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// A user attribute's name is not UTF-8. Cell names are strings.
    AttributeNameNotUtf8,
    /// A user attribute is named like a system one. The engine owns those cells.
    AttributeNameReserved(String),
    /// A required cell is missing, or has the wrong type or width.
    Malformed(&'static str),
}

/// The [`TypeId`] a value is stored under.
///
/// Matches on the variant, never on [`AttributeType`](crate::entity::AttributeType)'s
/// discriminant — the two numbering schemes differ. See the module docs.
pub const fn type_id_of(value: &AttributeValue) -> TypeId {
    match value {
        AttributeValue::Bool(_) => TypeId::BOOL,
        AttributeValue::Int(_) => TypeId::I32,
        AttributeValue::U64(_) => TypeId::U64,
        AttributeValue::U256(_) => TypeId::U256,
        AttributeValue::Decimal(_) => TypeId::DEC,
        AttributeValue::Bytes32(_) => TypeId::BYTES32,
        AttributeValue::Bytes(_) => TypeId::BYTES,
        AttributeValue::Str(_) => TypeId::STR,
        AttributeValue::EthereumAddress(_) => TypeId::ADDR,
        AttributeValue::EntityKey(_) => TypeId::KEY,
    }
}

/// The record key for an entity: its address, which is already 32 bytes.
pub const fn record_key(key: EntityAddress) -> RecordKey {
    RecordKey::from_entity(key)
}

/// A system cell name, as a `&str`.
///
/// The [`annotations`] constants are ASCII byte literals in this crate's own source,
/// so a failure here is a typo in a constant, not untrusted input.
fn system(name: &[u8]) -> CellName {
    CellName::from(core::str::from_utf8(name).expect("annotation names are ASCII"))
}

/// The cells of an entity record.
pub fn to_cells(entity: &Entity) -> Result<Vec<(CellName, Cell)>, RecordError> {
    let mut cells = alloc::vec![
        (
            system(annotations::KEY),
            Cell::attribute(TypeId::KEY, entity.key.to_vec())
        ),
        (
            system(annotations::CREATOR),
            Cell::attribute(TypeId::ADDR, entity.creator.to_vec())
        ),
        (
            system(annotations::OWNER),
            Cell::attribute(TypeId::ADDR, entity.owner.to_vec())
        ),
        (
            system(annotations::CREATED_AT_BLOCK),
            Cell::attribute(TypeId::U64, entity.created_at_block.to_be_bytes().to_vec()),
        ),
        (
            system(annotations::EXPIRATION),
            Cell::attribute(TypeId::U64, entity.expires_at.to_be_bytes().to_vec()),
        ),
        (
            system(annotations::CONTENT_TYPE),
            Cell::attribute(TypeId::STR, entity.content_type.clone()),
        ),
        (
            system(annotations::PAYLOAD),
            Cell::field(TypeId::BYTES, entity.payload.clone()),
        ),
        (
            CellName::from(CELL_LAST_MODIFIED),
            Cell::field(
                TypeId::U64,
                entity.last_modified_at_block.to_be_bytes().to_vec()
            ),
        ),
        (
            CellName::from(CELL_CREATION_FLAGS),
            Cell::field(TypeId::BOOL, alloc::vec![entity.creation_flags.bits()]),
        ),
    ];

    for Attribute { key, value } in &entity.attributes {
        let name = core::str::from_utf8(key).map_err(|_| RecordError::AttributeNameNotUtf8)?;
        // The engine owns every `$` cell. A user attribute that shadowed one would
        // silently overwrite lifecycle state, so reject rather than let it through.
        if key.first() == Some(&annotations::SYSTEM_PREFIX) {
            return Err(RecordError::AttributeNameReserved(String::from(name)));
        }
        cells.push((
            CellName::from(name),
            Cell::attribute(type_id_of(value), value.encode()),
        ));
    }
    Ok(cells)
}

/// Read a cell of an expected type, or say which one was wrong.
fn cell<'a>(
    record: &'a Record,
    name: &str,
    want: TypeId,
    missing: &'static str,
) -> Result<&'a [u8], RecordError> {
    record
        .cell(name)
        .filter(|cell| cell.type_id == want)
        .map(|cell| cell.value.as_slice())
        .ok_or(RecordError::Malformed(missing))
}

fn fixed<const N: usize>(bytes: &[u8], what: &'static str) -> Result<[u8; N], RecordError> {
    bytes.try_into().map_err(|_| RecordError::Malformed(what))
}

/// Rebuild an entity from its record.
pub fn from_record(record: &Record) -> Result<Entity, RecordError> {
    let key_bytes = cell(record, &system(annotations::KEY), TypeId::KEY, "$key")?;
    let created = cell(
        record,
        &system(annotations::CREATED_AT_BLOCK),
        TypeId::U64,
        "$createdAtBlock",
    )?;
    let expires = cell(
        record,
        &system(annotations::EXPIRATION),
        TypeId::U64,
        "$expiration",
    )?;
    let modified = cell(record, CELL_LAST_MODIFIED, TypeId::U64, CELL_LAST_MODIFIED)?;
    let flags_raw = cell(
        record,
        CELL_CREATION_FLAGS,
        TypeId::BOOL,
        CELL_CREATION_FLAGS,
    )?;
    let flags = flags_raw
        .first()
        .and_then(|bits| CreationFlags::from_bits(*bits))
        .ok_or(RecordError::Malformed(CELL_CREATION_FLAGS))?;

    let mut entity = Entity {
        key: fixed(key_bytes, "$key")?,
        creator: fixed(
            cell(
                record,
                &system(annotations::CREATOR),
                TypeId::ADDR,
                "$creator",
            )?,
            "$creator",
        )?,
        owner: fixed(
            cell(record, &system(annotations::OWNER), TypeId::ADDR, "$owner")?,
            "$owner",
        )?,
        created_at_block: u64::from_be_bytes(fixed(created, "$createdAtBlock")?),
        last_modified_at_block: u64::from_be_bytes(fixed(modified, CELL_LAST_MODIFIED)?),
        expires_at: u64::from_be_bytes(fixed(expires, "$expiration")?),
        creation_flags: flags,
        content_type: cell(
            record,
            &system(annotations::CONTENT_TYPE),
            TypeId::STR,
            "$contentType",
        )?
        .to_vec(),
        payload: cell(
            record,
            &system(annotations::PAYLOAD),
            TypeId::BYTES,
            "$payload",
        )?
        .to_vec(),
        attributes: Vec::new(),
    };

    // Everything that is not a system cell is a user attribute. Driven off the `$`
    // prefix rather than a list of known names, so a system cell added later cannot
    // start silently appearing in user attributes.
    for (name, stored) in &record.cells {
        if name.as_bytes().first() == Some(&annotations::SYSTEM_PREFIX)
            || stored.kind != CellKind::Attribute
        {
            continue;
        }
        let value = decode(stored).ok_or(RecordError::Malformed("user attribute"))?;
        entity
            .attributes
            .push(Attribute::new(name.as_bytes(), value));
    }
    Ok(entity)
}

/// Read a stored cell back into an [`AttributeValue`].
fn decode(stored: &Cell) -> Option<AttributeValue> {
    let bytes = stored.value.as_slice();
    Some(match stored.type_id {
        TypeId::BOOL => AttributeValue::Bool(*bytes.first()? != 0),
        TypeId::I32 => AttributeValue::Int(i32::from_be_bytes(bytes.try_into().ok()?)),
        TypeId::U64 => AttributeValue::U64(u64::from_be_bytes(bytes.try_into().ok()?)),
        TypeId::U256 => AttributeValue::U256(bytes.try_into().ok()?),
        TypeId::DEC => AttributeValue::Decimal(bytes.try_into().ok()?),
        TypeId::BYTES32 => AttributeValue::Bytes32(bytes.try_into().ok()?),
        TypeId::BYTES => AttributeValue::Bytes(bytes.to_vec()),
        TypeId::STR => AttributeValue::Str(String::from(core::str::from_utf8(bytes).ok()?)),
        TypeId::ADDR => AttributeValue::EthereumAddress(bytes.try_into().ok()?),
        TypeId::KEY => AttributeValue::EntityKey(bytes.try_into().ok()?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::AttributeType;
    use crate::store::RecordVersion;

    fn sample() -> Entity {
        Entity {
            key: [0x11; 32],
            creator: [0x22; 20],
            owner: [0x33; 20],
            created_at_block: 100,
            last_modified_at_block: 150,
            expires_at: 200,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: alloc::vec![1, 2, 3],
            attributes: alloc::vec![
                Attribute::new(b"level".to_vec(), AttributeValue::Int(-7)),
                Attribute::new(b"name".to_vec(), AttributeValue::Str(String::from("bob"))),
                Attribute::new(b"big".to_vec(), AttributeValue::U256([0xcd; 32])),
            ],
        }
    }

    fn as_record(entity: &Entity) -> Record {
        Record {
            key: record_key(entity.key),
            version: RecordVersion(1),
            cells: to_cells(entity).expect("maps"),
        }
    }

    #[test]
    fn an_entity_round_trips() {
        let entity = sample();
        let mut back = from_record(&as_record(&entity)).expect("reads back");
        // Cell order is the record's, not the original vector's.
        back.attributes.sort_by(|a, b| a.key.cmp(&b.key));
        let mut expected = entity.clone();
        expected.attributes.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(back, expected);
    }

    #[test]
    fn every_queryable_builtin_is_an_indexed_attribute() {
        // If one of these is stored as a field it silently stops matching, because
        // `Predicate::matches` only ever looks at attributes.
        let record = as_record(&sample());
        for name in [
            annotations::KEY,
            annotations::CREATOR,
            annotations::OWNER,
            annotations::CREATED_AT_BLOCK,
            annotations::EXPIRATION,
            annotations::CONTENT_TYPE,
        ] {
            let name = core::str::from_utf8(name).unwrap();
            let cell = record
                .cell(name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(cell.kind, CellKind::Attribute, "{name} must be queryable");
        }
    }

    #[test]
    fn the_payload_is_a_field_so_it_is_never_indexed() {
        let record = as_record(&sample());
        let payload = record
            .cell(core::str::from_utf8(annotations::PAYLOAD).unwrap())
            .unwrap();
        assert_eq!(payload.kind, CellKind::Field);
        assert_eq!(payload.type_id, TypeId::BYTES);
        // `bytes` is field-only in the store, so an attribute of it would be rejected.
        assert!(!TypeId::BYTES.indexable());
    }

    #[test]
    fn a_user_attribute_may_not_shadow_a_system_cell() {
        let mut entity = sample();
        entity
            .attributes
            .push(Attribute::new(b"$owner".to_vec(), AttributeValue::Int(1)));
        assert!(matches!(
            to_cells(&entity),
            Err(RecordError::AttributeNameReserved(_))
        ));
    }

    #[test]
    fn the_two_type_numberings_are_not_interchangeable() {
        // Pin the hazard: matching on an `AttributeType` discriminant to get a
        // `TypeId` would quietly store u64s as u256s.
        assert_ne!(AttributeType::U64 as u8, TypeId::U64.0);
        assert_ne!(AttributeType::U256 as u8, TypeId::U256.0);
        assert_eq!(type_id_of(&AttributeValue::U64(0)), TypeId::U64);
        assert_eq!(type_id_of(&AttributeValue::U256([0; 32])), TypeId::U256);
    }

    #[test]
    fn a_missing_or_mistyped_cell_is_reported_not_defaulted() {
        let mut record = as_record(&sample());
        record.cells.retain(|(name, _)| name != "$owner");
        assert_eq!(from_record(&record), Err(RecordError::Malformed("$owner")));

        let mut wrong = as_record(&sample());
        for (name, cell) in &mut wrong.cells {
            if name == "$owner" {
                *cell = Cell::attribute(TypeId::U64, alloc::vec![0; 8]);
            }
        }
        assert_eq!(from_record(&wrong), Err(RecordError::Malformed("$owner")));
    }

    #[test]
    fn negative_attributes_survive_the_round_trip() {
        // `encode` is two's complement and `index_bytes` is sign-biased; storing the
        // wrong one here would round-trip fine but sort wrongly, so pin the bytes.
        let entity = sample();
        let record = as_record(&entity);
        let level = record.cell("level").unwrap();
        assert_eq!(level.value, AttributeValue::Int(-7).encode());
        assert_ne!(level.value, AttributeValue::Int(-7).index_bytes());
    }
}
