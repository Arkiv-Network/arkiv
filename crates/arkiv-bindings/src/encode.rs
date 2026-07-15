//! Encoding helpers — Rust types → [`Operation`] / [`Attribute`] calldata.
//!
//! These methods are the primary interface for building `execute()` calldata.
//! Each op type has a constructor that accepts only the fields relevant to
//! that op and zeros the rest, so callers never need to know the full flat
//! struct layout.
//!
//! # Attributes
//!
//! [`Attribute`] constructors handle the `bytes32[4]` value packing for each
//! `valueType`. The contract requires attributes to be sorted ascending by
//! name for deterministic hashing and name-uniqueness enforcement;
//! [`Operation::create`] and [`Operation::update`] sort automatically.
//! Call [`Attribute::sort`] before passing a pre-built slice to the raw
//! struct if you bypass the factory methods.

use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256};
use eyre::{Result, bail};

use crate::{
    ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute, Ident32, Mime128, OP_CREATE, OP_DELETE,
    OP_EXPIRE, OP_EXTEND, OP_TRANSFER, OP_UPDATE, Operation,
};

// -----------------------------------------------------------------------------
// Operation constructors
// -----------------------------------------------------------------------------

impl Operation {
    /// Create a new entity.
    ///
    /// `btl` is blocks-to-live from the current block
    /// (`expiresAt = currentBlock + btl`). Attributes are sorted by name
    /// ascending automatically.
    pub fn create(
        btl: u32,
        payload: Bytes,
        content_type: Mime128,
        mut attributes: Vec<Attribute>,
    ) -> Self {
        Attribute::sort(&mut attributes);
        Self {
            operationType: OP_CREATE,
            entityKey: B256::ZERO,
            payload,
            contentType: content_type,
            attributes,
            btl,
            newOwner: Address::ZERO,
        }
    }

    /// Update an existing entity's payload, content type, and attributes.
    ///
    /// Attributes are sorted by name ascending automatically.
    pub fn update(
        entity_key: B256,
        payload: Bytes,
        content_type: Mime128,
        mut attributes: Vec<Attribute>,
    ) -> Self {
        Attribute::sort(&mut attributes);
        Self {
            operationType: OP_UPDATE,
            entityKey: entity_key,
            payload,
            contentType: content_type,
            attributes,
            ..Default::default()
        }
    }

    /// Extend an entity's expiry by `btl` blocks from the current block.
    pub fn extend(entity_key: B256, btl: u32) -> Self {
        Self {
            operationType: OP_EXTEND,
            entityKey: entity_key,
            btl,
            ..Default::default()
        }
    }

    /// Transfer entity ownership.
    pub fn transfer(entity_key: B256, new_owner: Address) -> Self {
        Self {
            operationType: OP_TRANSFER,
            entityKey: entity_key,
            newOwner: new_owner,
            ..Default::default()
        }
    }

    /// Delete an entity before its expiry.
    pub fn delete(entity_key: B256) -> Self {
        Self {
            operationType: OP_DELETE,
            entityKey: entity_key,
            ..Default::default()
        }
    }

    /// Remove an expired entity from storage.
    pub fn expire(entity_key: B256) -> Self {
        Self {
            operationType: OP_EXPIRE,
            entityKey: entity_key,
            ..Default::default()
        }
    }
}

// -----------------------------------------------------------------------------
// Attribute constructors
// -----------------------------------------------------------------------------

impl Attribute {
    /// Build a `ATTR_UINT` attribute. Value is right-aligned in `data[0]`.
    pub fn uint(name: Ident32, value: U256) -> Self {
        let mut v = [FixedBytes::ZERO; 4];
        v[0] = FixedBytes::from(value.to_be_bytes::<32>());
        Self {
            name: name.0,
            valueType: ATTR_UINT,
            value: v,
        }
    }

    /// Build an `ATTR_STRING` attribute from raw bytes.
    ///
    /// At most 128 bytes; the protocol treats the value as opaque — UTF-8
    /// is convention only. Bytes are left-aligned across the four slots.
    pub fn string(name: Ident32, value: &[u8]) -> Result<Self> {
        if value.len() > 128 {
            bail!("ATTR_STRING value exceeds 128 bytes ({})", value.len());
        }
        let mut v = [FixedBytes::ZERO; 4];
        for (i, chunk) in value.chunks(32).enumerate() {
            let mut buf = [0u8; 32];
            buf[..chunk.len()].copy_from_slice(chunk);
            v[i] = FixedBytes::from(buf);
        }
        Ok(Self {
            name: name.0,
            valueType: ATTR_STRING,
            value: v,
        })
    }

    /// Build an `ATTR_ENTITY_KEY` attribute. Key is stored in `data[0]`.
    pub fn entity_key(name: Ident32, key: B256) -> Self {
        let mut v = [FixedBytes::ZERO; 4];
        v[0] = key;
        Self {
            name: name.0,
            valueType: ATTR_ENTITY_KEY,
            value: v,
        }
    }

    /// Sort attributes by name ascending.
    ///
    /// The contract requires strict ascending order for deterministic hashing
    /// and to enforce name uniqueness. Called automatically by
    /// [`Operation::create`] and [`Operation::update`].
    pub fn sort(attrs: &mut [Self]) {
        attrs.sort_by_key(|a| a.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Ident32 {
        Ident32::encode(s).unwrap()
    }

    // -------------------------------------------------------------------------
    // Operation constructors
    // -------------------------------------------------------------------------

    #[test]
    fn create_sets_op_type_and_zeroes_entity_key() {
        let op = Operation::create(100, Bytes::new(), Mime128::default(), vec![]);
        assert_eq!(op.operationType, OP_CREATE);
        assert_eq!(op.entityKey, B256::ZERO);
        assert_eq!(op.btl, 100);
        assert_eq!(op.newOwner, Address::ZERO);
    }

    #[test]
    fn create_sorts_attributes() {
        let attrs = vec![
            Attribute::uint(name("z.attr"), U256::from(1)),
            Attribute::uint(name("a.attr"), U256::from(2)),
        ];
        let op = Operation::create(100, Bytes::new(), Mime128::default(), attrs);
        // a.attr < z.attr lexicographically
        assert!(op.attributes[0].name < op.attributes[1].name);
    }

    #[test]
    fn update_sets_op_type() {
        let op = Operation::update(
            B256::repeat_byte(1),
            Bytes::new(),
            Mime128::default(),
            vec![],
        );
        assert_eq!(op.operationType, OP_UPDATE);
        assert_eq!(op.entityKey, B256::repeat_byte(1));
        assert_eq!(op.btl, 0);
    }

    #[test]
    fn extend_sets_btl() {
        let key = B256::repeat_byte(2);
        let op = Operation::extend(key, 500);
        assert_eq!(op.operationType, OP_EXTEND);
        assert_eq!(op.entityKey, key);
        assert_eq!(op.btl, 500);
    }

    #[test]
    fn transfer_sets_new_owner() {
        let key = B256::repeat_byte(3);
        let owner = Address::repeat_byte(0xAB);
        let op = Operation::transfer(key, owner);
        assert_eq!(op.operationType, OP_TRANSFER);
        assert_eq!(op.newOwner, owner);
    }

    #[test]
    fn delete_and_expire_set_op_types() {
        let key = B256::repeat_byte(4);
        assert_eq!(Operation::delete(key).operationType, OP_DELETE);
        assert_eq!(Operation::expire(key).operationType, OP_EXPIRE);
    }

    // -------------------------------------------------------------------------
    // Attribute constructors
    // -------------------------------------------------------------------------

    #[test]
    fn uint_attr_packs_value_in_slot_zero() {
        let attr = Attribute::uint(name("count"), U256::from(42));
        assert_eq!(attr.valueType, ATTR_UINT);
        assert_eq!(
            attr.value[0],
            FixedBytes::from(U256::from(42).to_be_bytes::<32>())
        );
        assert_eq!(attr.value[1], FixedBytes::ZERO);
    }

    #[test]
    fn string_attr_packs_bytes_left_aligned() {
        let attr = Attribute::string(name("title"), b"hello").unwrap();
        assert_eq!(attr.valueType, ATTR_STRING);
        let expected: [u8; 32] = {
            let mut buf = [0u8; 32];
            buf[..5].copy_from_slice(b"hello");
            buf
        };
        assert_eq!(attr.value[0], FixedBytes::from(expected));
        assert_eq!(attr.value[1], FixedBytes::ZERO);
    }

    #[test]
    fn string_attr_rejects_over_128_bytes() {
        assert!(Attribute::string(name("big"), &[0u8; 129]).is_err());
    }

    #[test]
    fn string_attr_accepts_exactly_128_bytes() {
        assert!(Attribute::string(name("full"), &[0u8; 128]).is_ok());
    }

    #[test]
    fn entity_key_attr_packs_key_in_slot_zero() {
        let key = B256::repeat_byte(0x77);
        let attr = Attribute::entity_key(name("linked.to"), key);
        assert_eq!(attr.valueType, ATTR_ENTITY_KEY);
        assert_eq!(attr.value[0], key);
        assert_eq!(attr.value[1], FixedBytes::ZERO);
    }

    #[test]
    fn sort_orders_by_name_ascending() {
        let mut attrs = vec![
            Attribute::uint(name("z.last"), U256::ZERO),
            Attribute::uint(name("a.first"), U256::ZERO),
            Attribute::uint(name("m.mid"), U256::ZERO),
        ];
        Attribute::sort(&mut attrs);
        assert!(attrs[0].name < attrs[1].name);
        assert!(attrs[1].name < attrs[2].name);
    }
}
