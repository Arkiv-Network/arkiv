//! The key encodings of the three tries. Every byte here feeds the database
//! root, so a change is a chain fork.

use arkiv_interfaces::entity::{AttributeType, AttributeValue};
use arkiv_interfaces::primitives::{EntityAddress, EntityCreationNonce, UserAddress};

/// The key of an index in the index-of-indexes: `name ++ 0x00 ++ type id`.
/// Attribute names never contain `0x00`, so the separator keeps names of
/// different lengths apart.
pub fn index_id(attr: &[u8], ty: AttributeType) -> Vec<u8> {
    let mut out = Vec::with_capacity(attr.len() + 2);
    out.extend_from_slice(attr);
    out.push(0);
    out.push(ty as u8);
    out
}

/// A value's order-preserving key prefix inside its index. Byte order of the
/// result is the type's natural order.
///
/// Fixed-width types use their index bytes as is (sign-biased for the signed
/// ones). A string is escaped so that no encoding is a prefix of another
/// unless the strings are: `0x00` becomes `0x00 0x01`, and `0x00 0x00`
/// terminates. That keeps `"ab" < "abc"` regardless of what follows.
pub fn value_key(value: &AttributeValue) -> Vec<u8> {
    match value {
        AttributeValue::Str(s) => {
            let mut out = str_prefix_key(s);
            out.extend_from_slice(&[0, 0]);
            out
        }
        other => other.index_bytes(),
    }
}

/// The key prefix every string starting with `prefix` shares: the escaped
/// bytes without the terminator.
pub fn str_prefix_key(prefix: &str) -> Vec<u8> {
    let bytes = prefix.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 2);
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(1);
        }
    }
    out
}

/// The key of one `(value, entity)` entry in an index.
pub fn index_entry_key(value: &AttributeValue, entity: &EntityAddress) -> Vec<u8> {
    let mut out = value_key(value);
    out.extend_from_slice(entity);
    out
}

/// The entity key at the end of an index entry key.
pub fn entity_of_index_key(key: &[u8]) -> Option<EntityAddress> {
    let start = key.len().checked_sub(32)?;
    key[start..].try_into().ok()
}

/// The value stored on every index entry. Non-empty so a leaf never carries
/// the empty string, which some Merkle-Patricia readers treat as absent.
pub const INDEX_PRESENT: [u8; 1] = [1];

pub fn nonce_key(owner: &UserAddress) -> Vec<u8> {
    owner.to_vec()
}

pub fn nonce_value(nonce: EntityCreationNonce) -> Vec<u8> {
    nonce.get().to_be_bytes().to_vec()
}

pub fn nonce_from_value(bytes: &[u8]) -> Option<EntityCreationNonce> {
    let raw: [u8; 8] = bytes.try_into().ok()?;
    Some(EntityCreationNonce::new(u64::from_be_bytes(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_order_by_value_before_entity() {
        let a = index_entry_key(&AttributeValue::Str("ab".into()), &[0xff; 32]);
        let b = index_entry_key(&AttributeValue::Str("abc".into()), &[0x00; 32]);
        assert!(a < b);
    }

    #[test]
    fn string_prefix_is_a_key_prefix() {
        let full = value_key(&AttributeValue::Str("a\0b".into()));
        assert!(full.starts_with(&str_prefix_key("a\0")));
        assert!(full.starts_with(&str_prefix_key("a")));
        assert!(!full.starts_with(&str_prefix_key("b")));
        assert_eq!(full, vec![b'a', 0, 1, b'b', 0, 0]);
    }

    #[test]
    fn signed_values_order_numerically() {
        let neg = value_key(&AttributeValue::Int(-5));
        let zero = value_key(&AttributeValue::Int(0));
        let pos = value_key(&AttributeValue::Int(5));
        assert!(neg < zero && zero < pos);
    }
}
