//! Entity view layer: maps [`Entity`] to wire-ready structs with field projection.
//!
//! Ported from the `arkiv-db-engine` view layer — the wire shapes the Arkiv SDK
//! consumes. Everything here is pure Rust — no reth types, no JSON-RPC
//! plumbing. The RPC layer calls [`entity_data_from`] after fetching pages
//! from the query index.

use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_interfaces::entity::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Entity};
use eyre::Result;
use serde::{Deserialize, Serialize};

// ── Per-field projection ──────────────────────────────────────────────

/// Caller-supplied field inclusion options.
///
/// Each field defaults to `false` (not included) when the struct is present.
/// When no `IncludeData` is supplied by the caller, all fields are included
/// (see [`ResolvedIncludeData::all`]).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncludeData {
    pub key: Option<bool>,
    pub payload: Option<bool>,
    pub attributes: Option<bool>,
    pub content_type: Option<bool>,
    pub expiration: Option<bool>,
    pub owner: Option<bool>,
    pub creator: Option<bool>,
    pub created_at_block: Option<bool>,
    pub last_modified_at_block: Option<bool>,
    pub transaction_index_in_block: Option<bool>,
    pub operation_index_in_transaction: Option<bool>,
}

/// Resolved projection flags — computed once per request.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedIncludeData {
    pub key: bool,
    pub payload: bool,
    pub attributes: bool,
    pub content_type: bool,
    pub expiration: bool,
    pub owner: bool,
    pub creator: bool,
    pub created_at_block: bool,
    pub last_modified_at_block: bool,
    pub transaction_index_in_block: bool,
    pub operation_index_in_transaction: bool,
}

impl ResolvedIncludeData {
    pub fn all() -> Self {
        Self {
            key: true,
            payload: true,
            attributes: true,
            content_type: true,
            expiration: true,
            owner: true,
            creator: true,
            created_at_block: true,
            last_modified_at_block: true,
            transaction_index_in_block: true,
            operation_index_in_transaction: true,
        }
    }

    pub fn from_options(opt: Option<&IncludeData>) -> Self {
        match opt {
            None => Self::all(),
            Some(id) => Self {
                key: id.key.unwrap_or(false),
                payload: id.payload.unwrap_or(false),
                attributes: id.attributes.unwrap_or(false),
                content_type: id.content_type.unwrap_or(false),
                expiration: id.expiration.unwrap_or(false),
                owner: id.owner.unwrap_or(false),
                creator: id.creator.unwrap_or(false),
                created_at_block: id.created_at_block.unwrap_or(false),
                last_modified_at_block: id.last_modified_at_block.unwrap_or(false),
                transaction_index_in_block: id.transaction_index_in_block.unwrap_or(false),
                operation_index_in_transaction: id.operation_index_in_transaction.unwrap_or(false),
            },
        }
    }
}

// ── Wire types ────────────────────────────────────────────────────────

/// Wire attribute in RPC responses.
///
/// `value`'s encoding depends on `value_type`:
/// - `ATTR_UINT` → decimal `U256` string (e.g. `"42"`)
/// - `ATTR_STRING` → UTF-8 string
/// - `ATTR_ENTITY_KEY` → `0x`-prefixed hex of the 32-byte key
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribute {
    pub key: String,
    pub value_type: u8,
    pub value: String,
}

/// Per-entity payload in `arkiv_query` responses.
///
/// All fields are optional and skipped on serialization when `None` —
/// opting out via `include_data` omits them from the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Bytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<Address>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex",
        deserialize_with = "de_u64_flexible"
    )]
    pub created_at_block: Option<u64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex",
        deserialize_with = "de_u64_flexible"
    )]
    pub last_modified_at_block: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_index_in_block: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_index_in_transaction: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub attributes: Vec<Attribute>,
}

// ── Mapping ───────────────────────────────────────────────────────────

/// Map a raw [`Entity`] to a wire-ready `EntityData` according to the
/// resolved projection flags.
pub fn entity_data_from(e: Entity, inc: &ResolvedIncludeData) -> EntityData {
    let attributes = if inc.attributes {
        e.attributes
            .into_iter()
            .map(|a| Attribute {
                key: String::from_utf8_lossy(&a.key).into_owned(),
                value_type: a.value_type,
                value: format_attribute_value(a.value_type, &a.value),
            })
            .collect()
    } else {
        Vec::new()
    };

    EntityData {
        key: inc.key.then_some(B256::from(e.key)),
        value: inc.payload.then(|| Bytes::from(e.payload)),
        content_type: inc
            .content_type
            .then(|| String::from_utf8_lossy(&e.content_type).into_owned()),
        expires_at: inc.expiration.then_some(e.expires_at),
        owner: inc.owner.then_some(Address::from(e.owner)),
        creator: inc.creator.then_some(Address::from(e.creator)),
        created_at_block: inc.created_at_block.then_some(e.created_at_block),
        last_modified_at_block: inc
            .last_modified_at_block
            .then_some(e.last_modified_at_block),
        transaction_index_in_block: inc.transaction_index_in_block.then_some(0),
        operation_index_in_transaction: inc.operation_index_in_transaction.then_some(0),
        attributes,
    }
}

/// Format a raw attribute value as a human-readable string for the wire.
pub fn format_attribute_value(value_type: u8, bytes: &[u8]) -> String {
    match value_type {
        ATTR_UINT => U256::from_be_slice(bytes).to_string(),
        ATTR_STRING => String::from_utf8_lossy(bytes).into_owned(),
        ATTR_ENTITY_KEY => alloy_primitives::hex::encode_prefixed(bytes),
        _ => String::new(),
    }
}

/// Parse a hex cursor string (`"0x1a"`) into a `u64` page cursor.
pub fn parse_cursor(s: Option<&str>) -> Result<Option<u64>> {
    match s {
        None => Ok(None),
        Some(c) => {
            let stripped = c.strip_prefix("0x").unwrap_or(c);
            let n = u64::from_str_radix(stripped, 16)
                .map_err(|e| eyre::eyre!("invalid cursor {c:?}: {e}"))?;
            Ok(Some(n))
        }
    }
}

// ── Serde helpers (pub for re-use in rpc.rs) ─────────────────────────

pub fn ser_u64_hex<S: serde::Serializer>(v: &u64, s: S) -> core::result::Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{v:x}"))
}

pub fn ser_opt_u64_hex<S: serde::Serializer>(
    v: &Option<u64>,
    s: S,
) -> core::result::Result<S::Ok, S::Error> {
    match v {
        Some(n) => s.serialize_str(&format!("0x{n:x}")),
        None => s.serialize_none(),
    }
}

pub fn de_u64_flexible<'de, D>(de: D) -> core::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Hex(String),
    }
    let opt: Option<Either> = Option::deserialize(de)?;
    match opt {
        None => Ok(None),
        Some(Either::Num(n)) => Ok(Some(n)),
        Some(Either::Hex(s)) => {
            let stripped = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(&s);
            u64::from_str_radix(stripped, 16)
                .map(Some)
                .map_err(|e| D::Error::custom(format!("invalid hex u64 {s:?}: {e}")))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uint_value_is_decimal_string() {
        let bytes = U256::from(123_456_789u64).to_be_bytes::<32>();
        assert_eq!(format_attribute_value(ATTR_UINT, &bytes), "123456789");
    }

    #[test]
    fn format_string_value_is_utf8() {
        assert_eq!(format_attribute_value(ATTR_STRING, b"hello"), "hello");
    }

    #[test]
    fn format_entity_key_value_is_prefixed_hex() {
        let mut k = [0u8; 32];
        k[0] = 0xab;
        k[1] = 0xcd;
        assert_eq!(
            format_attribute_value(ATTR_ENTITY_KEY, &k),
            "0xabcd000000000000000000000000000000000000000000000000000000000000"
        );
    }

    fn test_entity() -> Entity {
        Entity {
            key: [0u8; 32],
            creator: [0u8; 20],
            owner: [0u8; 20],
            created_at_block: 0,
            last_modified_at_block: 0,
            expires_at: 0,
            content_type: Vec::new(),
            payload: Vec::new(),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn key_is_omitted_unless_requested() {
        let inc = ResolvedIncludeData::from_options(Some(&IncludeData {
            attributes: Some(true),
            ..Default::default()
        }));
        let data = entity_data_from(test_entity(), &inc);
        assert_eq!(data.key, None);
        let json = serde_json::to_value(&data).expect("serialize");
        assert!(json.get("key").is_none());

        let all = entity_data_from(test_entity(), &ResolvedIncludeData::all());
        assert_eq!(all.key, Some(B256::ZERO));
    }

    #[test]
    fn attribute_serializes_camel_case() {
        let attr = Attribute {
            key: "score".to_string(),
            value_type: ATTR_UINT,
            value: "42".to_string(),
        };
        let json = serde_json::to_value(&attr).expect("serialize");
        assert_eq!(json["key"], "score");
        assert_eq!(json["valueType"], 1);
        assert_eq!(json["value"], "42");
        assert!(json.get("value_type").is_none());
    }

    #[test]
    fn block_fields_serialize_as_hex_strings() {
        let mut e = test_entity();
        e.created_at_block = 26;
        e.last_modified_at_block = 255;
        let json = serde_json::to_value(entity_data_from(e, &ResolvedIncludeData::all())).unwrap();
        assert_eq!(json["createdAtBlock"], "0x1a");
        assert_eq!(json["lastModifiedAtBlock"], "0xff");
    }
}
