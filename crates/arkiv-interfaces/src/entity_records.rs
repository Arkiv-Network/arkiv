//! An [`Entity`] as a GolemDB [`Record`] and back, and reading one by its key ([`read`]).
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
//! One entity is one record, keyed by the entity address. An account's key holds
//! an account's record, so [`record_key`] has no entity record key for it.
//!
//! Every system cell is named by its [`annotations`] constant: the API spec's name,
//! `$`-prefixed.
//!
//! | cell                  | kind      | type     | from             |
//! |-----------------------|-----------|----------|------------------|
//! | `$key`                | attribute | `KEY`    | `key`            |
//! | `$creator`            | attribute | `ADDR`   | `creator`        |
//! | `$owner`              | attribute | `ADDR`   | `owner`          |
//! | `$createdAt`          | **field** | `U64`    | `created_at`     |
//! | `$expiresAt`          | attribute | `U64`    | `expires_at`     |
//! | `$updatedAt`          | **field** | `U64`    | `updated_at`     |
//! | `$creationFlags`      | **field** | `BYTES`* | `creation_flags` |
//! | *user attribute name* | attribute if `indexed`, else field | per value | `attributes` |
//!
//! Attributes are indexed and queryable; fields are not. The split follows the
//! language exactly: the [`BuiltIn`](crate::query::BuiltIn) keys are attributes, a
//! user attribute is whichever its writer flagged, and everything else is a field,
//! so nothing is paying for an index it cannot use.
//!
//! \* `creation_flags` is one byte holding up to `0b11`, so it is a one-byte `BYTES`
//! value. Not `BOOL`: a store may refuse a `BOOL` other than 0 or 1, as GolemDB
//! does, and an entity with `permissionless_extension` would then fail to write.
//!
//! # Two numbering schemes that must not be confused
//!
//! [`AttributeType`] and [`TypeId`] both number types, and they **disagree** —
//! `AttributeType::U64` is 3 where [`TypeId::U64`] is 8. Conversion goes through
//! [`type_id_for`], which matches on the variant, never on a discriminant.

use alloc::string::String;
use alloc::vec::Vec;

use crate::constants::ENTITY_MAX_ATTRIBUTES;
use crate::entity::{Attribute, AttributeType, AttributeValue, CreationFlags, Entity, annotations};
use crate::primitives::EntityAddress;
use crate::statemanager::EntityUpdates;
use crate::store::{
    Cell, CellKind, CellName, ReadTarget, Record, RecordKey, RecordMeta, Store, StoreError, TypeId,
};

/// The system cells of the layout above, which every entity record holds.
const SYSTEM_CELLS: usize = 7;

/// Of the [`SYSTEM_CELLS`], the attributes: `$key`, `$creator`, `$owner` and
/// `$expiresAt`.
const SYSTEM_ATTRIBUTES: usize = 4;

/// The most cells an entity record holds: its system cells and one per
/// attribute. An account record holds a few fields, so this caps every record,
/// and GolemDB's genesis states it as `record_limits.max_cells`.
pub const MAX_RECORD_CELLS: usize = SYSTEM_CELLS + ENTITY_MAX_ATTRIBUTES;

/// The most of a record's cells that are attributes, every user attribute
/// indexed: GolemDB's `record_limits.max_indexed_cells`.
pub const MAX_RECORD_INDEXED_CELLS: usize = SYSTEM_ATTRIBUTES + ENTITY_MAX_ATTRIBUTES;

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
pub const fn type_id_of(value: &AttributeValue) -> TypeId {
    type_id_for(value.attr_type())
}

/// The [`TypeId`] an attribute type is stored under.
///
/// Matches on the variant, never on [`AttributeType`]'s discriminant — the two
/// numbering schemes differ. See the module docs.
pub const fn type_id_for(attr_type: AttributeType) -> TypeId {
    match attr_type {
        AttributeType::Bool => TypeId::BOOL,
        AttributeType::Int => TypeId::I32,
        AttributeType::U64 => TypeId::U64,
        AttributeType::U256 => TypeId::U256,
        AttributeType::Decimal => TypeId::DEC,
        AttributeType::Bytes32 => TypeId::BYTES32,
        AttributeType::Bytes => TypeId::BYTES,
        AttributeType::Str => TypeId::STR,
        AttributeType::EthereumAddress => TypeId::ADDR,
        AttributeType::EntityKey => TypeId::KEY,
    }
}

/// The record key for an entity: its address, which is already 32 bytes. `None`
/// for a reserved key, where no entity is read or written.
pub const fn record_key(key: EntityAddress) -> Option<RecordKey> {
    if is_reserved_key(key) {
        None
    } else {
        Some(RecordKey::from_entity(key))
    }
}

/// Account records must never be interpreted as entities.
pub const fn is_reserved_key(key: EntityAddress) -> bool {
    is_account_key(key)
}

/// Whether `key` is an account's: an address left-padded with twelve zero bytes.
/// A caller can name any key; at an account's there is no entity.
pub const fn is_account_key(key: EntityAddress) -> bool {
    RecordKey::from_entity(key).as_address().is_some()
}

/// The entity at `key` as of `target`, or `None` if there is none.
pub fn read<S, E>(store: &S, target: ReadTarget, key: EntityAddress) -> Result<Option<Entity>, E>
where
    S: Store + ?Sized,
    E: From<StoreError> + From<RecordError>,
{
    let Some(record_key) = record_key(key) else {
        return Ok(None);
    };
    let record = store.get(target, record_key, None, None)?.into_value();
    Ok(record.as_ref().map(from_record).transpose()?)
}

/// A system cell name, as a `&str`.
///
/// The [`annotations`] constants are ASCII byte literals in this crate's own source,
/// so a failure here is a typo in a constant, not untrusted input.
pub(crate) fn system(name: &[u8]) -> CellName {
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
            system(annotations::CREATED_AT),
            Cell::field(TypeId::U64, entity.created_at.to_be_bytes().to_vec()),
        ),
        (
            system(annotations::EXPIRES_AT),
            Cell::attribute(TypeId::U64, entity.expires_at.to_be_bytes().to_vec()),
        ),
        (
            system(annotations::UPDATED_AT),
            Cell::field(TypeId::U64, entity.updated_at.to_be_bytes().to_vec()),
        ),
        (
            system(annotations::CREATION_FLAGS),
            Cell::field(TypeId::BYTES, alloc::vec![entity.creation_flags.bits()]),
        ),
    ];

    for Attribute {
        key,
        value,
        indexed,
    } in &entity.attributes
    {
        let name = core::str::from_utf8(key).map_err(|_| RecordError::AttributeNameNotUtf8)?;
        // The engine owns every `$` cell. A user attribute that shadowed one would
        // silently overwrite lifecycle state, so reject rather than let it through.
        if key.first() == Some(&annotations::SYSTEM_PREFIX) {
            return Err(RecordError::AttributeNameReserved(String::from(name)));
        }
        let cell = if *indexed {
            Cell::attribute(type_id_of(value), value.encode())
        } else {
            Cell::field(type_id_of(value), value.encode())
        };
        cells.push((CellName::from(name), cell));
    }
    Ok(cells)
}

/// The system cells and the record's `#meta`: a read projected on them gives
/// [`from_record`] an entity without its attributes, and the record's size.
/// What an op needs that reads an entity only to authorize and price itself.
pub fn system_projection() -> Vec<CellName> {
    [
        annotations::KEY,
        annotations::CREATOR,
        annotations::OWNER,
        annotations::CREATED_AT,
        annotations::EXPIRES_AT,
        annotations::UPDATED_AT,
        annotations::CREATION_FLAGS,
    ]
    .into_iter()
    .map(system)
    .chain([CellName::from(RecordMeta::CELL)])
    .collect()
}

/// The system cells `updates` sets, as [`to_cells`] encodes them. An update
/// that leaves the attributes alone writes these and nothing else. The key
/// never changes, so it is never among them.
pub fn system_cells_set_by(updates: &EntityUpdates) -> Result<Vec<(CellName, Cell)>, RecordError> {
    let mut entity = Entity::default();
    updates.apply_to(&mut entity);
    let set: Vec<CellName> = [
        (annotations::CREATOR, updates.creator.is_some()),
        (annotations::OWNER, updates.owner.is_some()),
        (annotations::CREATED_AT, updates.created_at.is_some()),
        (annotations::EXPIRES_AT, updates.expires_at.is_some()),
        (annotations::UPDATED_AT, updates.updated_at.is_some()),
        (
            annotations::CREATION_FLAGS,
            updates.creation_flags.is_some(),
        ),
    ]
    .into_iter()
    .filter(|(_, is_set)| *is_set)
    .map(|(name, _)| system(name))
    .collect();
    Ok(to_cells(&entity)?
        .into_iter()
        .filter(|(name, _)| set.contains(name))
        .collect())
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
        &system(annotations::CREATED_AT),
        TypeId::U64,
        "$createdAt",
    )?;
    let expires = cell(
        record,
        &system(annotations::EXPIRES_AT),
        TypeId::U64,
        "$expiresAt",
    )?;
    let updated = cell(
        record,
        &system(annotations::UPDATED_AT),
        TypeId::U64,
        "$updatedAt",
    )?;
    let flags_raw = cell(
        record,
        &system(annotations::CREATION_FLAGS),
        TypeId::BYTES,
        "$creationFlags",
    )?;
    let [bits] = fixed::<1>(flags_raw, "$creationFlags")?;
    let flags = CreationFlags::from_bits(bits).ok_or(RecordError::Malformed("$creationFlags"))?;

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
        created_at: u64::from_be_bytes(fixed(created, "$createdAt")?),
        updated_at: u64::from_be_bytes(fixed(updated, "$updatedAt")?),
        expires_at: u64::from_be_bytes(fixed(expires, "$expiresAt")?),
        creation_flags: flags,
        attributes: Vec::new(),
    };

    // Everything that is not a system cell is a user attribute, and its cell kind
    // is its indexed flag. Driven off the `$` prefix rather than a list of known
    // names, so a system cell added later cannot start silently appearing in user
    // attributes.
    for (name, stored) in &record.cells {
        if name.as_bytes().first() == Some(&annotations::SYSTEM_PREFIX) {
            continue;
        }
        let value = decode(stored).ok_or(RecordError::Malformed("user attribute"))?;
        entity.attributes.push(Attribute {
            key: name.as_bytes().to_vec(),
            value,
            indexed: stored.kind == CellKind::Attribute,
        });
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
            created_at: 100,
            updated_at: 150,
            expires_at: 200,
            creation_flags: CreationFlags::NONE,
            attributes: alloc::vec![
                Attribute::new(b"level".to_vec(), AttributeValue::Int(-7)),
                Attribute::new(b"name".to_vec(), AttributeValue::Str(String::from("bob"))),
                Attribute::new(b"big".to_vec(), AttributeValue::U256([0xcd; 32])),
                Attribute::unindexed(b"title".to_vec(), AttributeValue::Str(String::from("hi"))),
                Attribute::unindexed(
                    b"body".to_vec(),
                    AttributeValue::Bytes(alloc::vec![1, 2, 3])
                ),
                Attribute::unindexed(b"thumb".to_vec(), AttributeValue::Bytes(alloc::vec![])),
            ],
        }
    }

    fn as_record(entity: &Entity) -> Record {
        Record {
            key: record_key(entity.key).unwrap(),
            version: RecordVersion(1),
            cells: to_cells(entity).expect("maps"),
            meta: None,
        }
    }

    /// An entity with every attribute it may have, all indexed, fills the
    /// record caps exactly, so the system cell counts are `to_cells`'s own.
    #[test]
    fn a_full_entity_fills_the_record_caps() {
        let entity = Entity {
            attributes: (0..ENTITY_MAX_ATTRIBUTES)
                .map(|i| {
                    Attribute::new(alloc::format!("a{i}").into_bytes(), AttributeValue::U64(0))
                })
                .collect(),
            ..sample()
        };
        let cells = to_cells(&entity).expect("maps");
        let indexed = cells
            .iter()
            .filter(|(_, cell)| matches!(cell.kind, CellKind::Attribute))
            .count();
        assert_eq!(cells.len(), MAX_RECORD_CELLS);
        assert_eq!(indexed, MAX_RECORD_INDEXED_CELLS);
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
            annotations::EXPIRES_AT,
        ] {
            let name = core::str::from_utf8(name).unwrap();
            let cell = record
                .cell(name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(cell.kind, CellKind::Attribute, "{name} must be queryable");
        }
    }

    #[test]
    fn every_projection_only_system_cell_is_a_field() {
        // The API spec returns these but does not query them; an attribute would
        // pay for an index nothing can use.
        let record = as_record(&sample());
        for name in [
            annotations::CREATED_AT,
            annotations::UPDATED_AT,
            annotations::CREATION_FLAGS,
        ] {
            let name = core::str::from_utf8(name).unwrap();
            let cell = record
                .cell(name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(cell.kind, CellKind::Field, "{name} must not be indexed");
        }
    }

    #[test]
    fn the_indexed_flag_picks_the_cell_kind() {
        // An unindexed attribute stored as an attribute cell would be silently
        // queryable; an indexed one stored as a field would silently stop matching.
        let record = as_record(&sample());
        assert_eq!(record.cell("level").unwrap().kind, CellKind::Attribute);
        assert_eq!(record.cell("title").unwrap().kind, CellKind::Field);
        let body = record.cell("body").unwrap();
        assert_eq!(body.kind, CellKind::Field);
        assert_eq!(body.type_id, TypeId::BYTES);
        // `bytes` is field-only in the store, so an attribute of it would be rejected.
        assert!(!TypeId::BYTES.indexable());
    }

    #[test]
    fn an_entity_needs_no_user_attributes_at_all() {
        let mut entity = sample();
        entity.attributes.clear();
        assert_eq!(from_record(&as_record(&entity)), Ok(entity));
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

    /// No entity is read or written at a reserved key: an account's, or the fee
    /// schedule's. Any other key is an entity's own.
    #[test]
    fn reserved_keys_have_no_entity_record() {
        let mut account = [0u8; 32];
        account[12..].copy_from_slice(&[0xaa; 20]);
        for key in [account] {
            assert!(is_reserved_key(key));
            assert_eq!(record_key(key), None);
        }
        let mut near = [0x7b; 32];
        near[31] ^= 1;
        for key in [[0x11; 32], near] {
            assert!(!is_reserved_key(key));
            assert_eq!(record_key(key), Some(RecordKey::from_entity(key)));
        }
    }
}
