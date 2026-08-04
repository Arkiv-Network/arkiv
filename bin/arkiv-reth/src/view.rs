//! The `arkiv_query` wire shapes: [`Select`] in, [`EntityData`] out.
//!
//! ## What "projection" means here
//!
//! A stored [`Entity`] has more on it than most callers want: the payload can
//! be 128 KiB, and a query answers a whole page of them. So a read names which
//! fields it wants — its **projection** — and everything else is left off the
//! response entirely rather than sent as null. A field that was asked for is a
//! *projected field*; the rest are absent, and `serde` skips them, so a caller
//! can tell "I did not ask for this" from "this has no value".
//!
//! Pure data mapping — no reth types, no JSON-RPC plumbing. The RPC layer calls
//! [`entity_data_from`] once per matched entity.
//!
//! Two rules from the spec drive everything here:
//!
//! - **Projections are opt-in.** `select` defaults to `{"key": true}`; every other
//!   field is off unless asked for. Returning a payload nobody asked for is the
//!   expensive mistake, so the default is the cheap one.
//! - **Values are typed on the wire too.** An attribute reports its `type` using
//!   the same tag the query language uses (`i32`, `u256`, `dec`, `str`, `addr`,
//!   `key`, `bytes32`, `bool`), and its `value` in that type's JSON encoding —
//!   a `u256` as a hex quantity, an `i32` as a JSON number, a `dec` as a decimal
//!   string, a `bool` as a JSON bool. A value read from a response therefore
//!   drops straight back into a predicate.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_interfaces::entity::{self, AttributeValue, DECIMAL_SCALE, Entity};
use serde::{Deserialize, Serialize};

// ── Projection: what the caller asked for ─────────────────────────────

/// The `select` object. Every field is optional and defaults to **off**; an
/// absent `select` means [`Projection::default`], i.e. `{"key": true}`.
///
/// Unknown fields are rejected rather than ignored — a typo'd projection that
/// silently returned nothing would look like missing data.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Select {
    pub key: Option<bool>,
    pub owner: Option<bool>,
    pub creator: Option<bool>,
    pub created_at: Option<bool>,
    pub updated_at: Option<bool>,
    pub expires_at: Option<bool>,
    pub creation_flags: Option<bool>,
    pub content_type: Option<bool>,
    pub payload: Option<bool>,
    pub attribute_schema: Option<bool>,
    pub attributes: Option<AttributeSelect>,
}

/// `attributes` is either a bare `true` (all of them) or a `{name: true}` map
/// naming the subset to return.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AttributeSelect {
    All(bool),
    Named(BTreeMap<String, bool>),
}

/// Which attributes to return with their values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AttributeProjection {
    /// None — the default.
    #[default]
    Off,
    /// Every attribute the entity carries.
    All,
    /// Only these names, in the order the entity stores them.
    Named(BTreeSet<String>),
}

/// A resolved `select` — plain booleans, computed once per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub key: bool,
    pub owner: bool,
    pub creator: bool,
    pub created_at: bool,
    pub updated_at: bool,
    pub expires_at: bool,
    pub creation_flags: bool,
    pub content_type: bool,
    pub payload: bool,
    pub attribute_schema: bool,
    pub attributes: AttributeProjection,
}

impl Default for Projection {
    /// The spec's default: the key and nothing else.
    fn default() -> Self {
        Self {
            key: true,
            owner: false,
            creator: false,
            created_at: false,
            updated_at: false,
            expires_at: false,
            creation_flags: false,
            content_type: false,
            payload: false,
            attribute_schema: false,
            attributes: AttributeProjection::Off,
        }
    }
}

impl Projection {
    /// Resolve a caller's `select`, or the default when it is absent.
    ///
    /// Errors describe the one field this version cannot serve.
    pub fn resolve(select: Option<&Select>) -> Result<Self, String> {
        let Some(select) = select else {
            return Ok(Self::default());
        };
        Ok(Self {
            key: select.key.unwrap_or(false),
            owner: select.owner.unwrap_or(false),
            creator: select.creator.unwrap_or(false),
            created_at: select.created_at.unwrap_or(false),
            updated_at: select.updated_at.unwrap_or(false),
            expires_at: select.expires_at.unwrap_or(false),
            creation_flags: select.creation_flags.unwrap_or(false),
            content_type: select.content_type.unwrap_or(false),
            payload: select.payload.unwrap_or(false),
            attribute_schema: select.attribute_schema.unwrap_or(false),
            attributes: match &select.attributes {
                None | Some(AttributeSelect::All(false)) => AttributeProjection::Off,
                Some(AttributeSelect::All(true)) => AttributeProjection::All,
                Some(AttributeSelect::Named(named)) => AttributeProjection::Named(
                    named
                        .iter()
                        .filter(|(_, wanted)| **wanted)
                        .map(|(name, _)| name.clone())
                        .collect(),
                ),
            },
        })
    }

    /// A stable byte encoding of this projection, for binding a cursor to the
    /// request that produced it. Any change a caller could make to `select` has
    /// to change these bytes.
    pub fn fingerprint(&self) -> Vec<u8> {
        let mut out = vec![
            u8::from(self.key),
            u8::from(self.owner),
            u8::from(self.creator),
            u8::from(self.created_at),
            u8::from(self.updated_at),
            u8::from(self.expires_at),
            u8::from(self.creation_flags),
            u8::from(self.content_type),
            u8::from(self.payload),
            u8::from(self.attribute_schema),
        ];
        match &self.attributes {
            AttributeProjection::Off => out.push(0),
            AttributeProjection::All => out.push(1),
            AttributeProjection::Named(names) => {
                out.push(2);
                // The set is ordered, so the encoding is too.
                for name in names {
                    out.extend_from_slice(name.as_bytes());
                    out.push(0);
                }
            }
        }
        out
    }
}

// ── Wire types ────────────────────────────────────────────────────────

/// One attribute's name and type, without its value — `select.attributeSchema`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
}

/// An entity's creation flags, decoded.
///
/// The named booleans are what callers branch on; `raw` carries the byte itself
/// so a client still sees bits this node's version does not name — six are
/// reserved, and booleans alone would silently drop whatever a later upgrade
/// defines.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreationFlags {
    pub readonly: bool,
    pub permissionless_extension: bool,
    pub raw: u8,
}

impl CreationFlags {
    /// Project the protocol type onto the wire shape.
    pub fn from_flags(flags: entity::CreationFlags) -> Self {
        Self {
            readonly: flags.is_readonly(),
            permissionless_extension: flags.allows_permissionless_extension(),
            raw: flags.bits(),
        }
    }
}

/// One attribute with its value — `select.attributes`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttributeEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
    pub value: serde_json::Value,
}

/// One entity in an `arkiv_query` response. Every field is omitted unless the
/// caller selected it.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EntityData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<Address>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex"
    )]
    pub created_at: Option<u64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex"
    )]
    pub updated_at: Option<u64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex"
    )]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_flags: Option<CreationFlags>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Bytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribute_schema: Option<Vec<SchemaEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<AttributeEntry>>,
}

// ── Mapping ───────────────────────────────────────────────────────────

/// Project one entity onto the wire.
pub fn entity_data_from(entity: Entity, projection: &Projection) -> EntityData {
    let wanted = |name: &[u8]| match &projection.attributes {
        AttributeProjection::Off => false,
        AttributeProjection::All => true,
        AttributeProjection::Named(names) => std::str::from_utf8(name)
            .map(|name| names.contains(name))
            .unwrap_or(false),
    };

    let attribute_schema = projection.attribute_schema.then(|| {
        entity
            .attributes
            .iter()
            .map(|attribute| SchemaEntry {
                name: String::from_utf8_lossy(&attribute.key).into_owned(),
                value_type: attribute.value.attr_type().name().to_string(),
            })
            .collect()
    });

    let attributes = (projection.attributes != AttributeProjection::Off).then(|| {
        entity
            .attributes
            .iter()
            .filter(|attribute| wanted(&attribute.key))
            .map(|attribute| AttributeEntry {
                name: String::from_utf8_lossy(&attribute.key).into_owned(),
                value_type: attribute.value.attr_type().name().to_string(),
                value: attribute_value_json(&attribute.value),
            })
            .collect()
    });

    EntityData {
        key: projection.key.then_some(B256::from(entity.key)),
        owner: projection.owner.then_some(Address::from(entity.owner)),
        creator: projection.creator.then_some(Address::from(entity.creator)),
        created_at: projection.created_at.then_some(entity.created_at_block),
        updated_at: projection
            .updated_at
            .then_some(entity.last_modified_at_block),
        expires_at: projection.expires_at.then_some(entity.expires_at),
        creation_flags: projection
            .creation_flags
            .then(|| CreationFlags::from_flags(entity.creation_flags)),
        content_type: projection
            .content_type
            .then(|| String::from_utf8_lossy(&entity.content_type).into_owned()),
        payload: projection.payload.then(|| Bytes::from(entity.payload)),
        attribute_schema,
        attributes,
    }
}

/// An attribute value in its per-type JSON encoding.
///
/// The encodings are chosen so a value survives a round trip through JSON
/// without losing range or precision: `u256` is a hex quantity (a JSON number
/// would overflow a double), `dec` is a decimal string (likewise), `i32` is a
/// plain number, and the fixed-width byte types are `0x` data at full width.
pub fn attribute_value_json(value: &AttributeValue) -> serde_json::Value {
    match value {
        AttributeValue::Bool(flag) => serde_json::Value::from(*flag),
        AttributeValue::Int(number) => serde_json::Value::from(*number),
        // u64 is a QUANTITY, not a JSON number: only i32 is small enough to be
        // safe in a JS number, so everything wider goes over as 0x hex.
        AttributeValue::U64(number) => serde_json::Value::from(format!("{number:#x}")),
        AttributeValue::U256(word) => serde_json::Value::from(hex_quantity(*word)),
        AttributeValue::Decimal(word) => serde_json::Value::from(format_decimal(*word)),
        AttributeValue::Str(text) => serde_json::Value::from(text.clone()),
        AttributeValue::Bytes32(word) | AttributeValue::EntityKey(word) => {
            serde_json::Value::from(alloy_primitives::hex::encode_prefixed(word))
        }
        AttributeValue::EthereumAddress(address) => {
            serde_json::Value::from(alloy_primitives::hex::encode_prefixed(address))
        }
        AttributeValue::Bytes(bytes) => {
            serde_json::Value::from(alloy_primitives::hex::encode_prefixed(bytes))
        }
    }
}

/// A 256-bit word as an `eth_*`-style hex quantity: minimal digits, `0x0` for
/// zero.
fn hex_quantity(word: [u8; 32]) -> String {
    format!("{:#x}", U256::from_be_bytes(word))
}

/// Render a fixed-scale `dec` (a two's-complement `int256` scaled by
/// `10^DECIMAL_SCALE`) as a plain decimal string, without trailing zeros.
fn format_decimal(word: [u8; 32]) -> String {
    let (sign, magnitude) = split_sign(word);
    let scale = U256::from(10u8).pow(U256::from(DECIMAL_SCALE));
    let whole = magnitude / scale;
    match trimmed_fraction(magnitude % scale) {
        Some(fraction) => format!("{sign}{whole}.{fraction}"),
        None => format!("{sign}{whole}"),
    }
}

/// Split a two's-complement `int256` into its sign prefix and absolute value.
fn split_sign(word: [u8; 32]) -> (&'static str, U256) {
    let raw = U256::from_be_bytes(word);
    if word[0] & 0x80 == 0 {
        ("", raw)
    } else {
        ("-", U256::ZERO.wrapping_sub(raw))
    }
}

/// The fractional digits, zero-padded to the scale and stripped of trailing
/// zeros — `None` when nothing is left, i.e. the value is a whole number.
fn trimmed_fraction(remainder: U256) -> Option<String> {
    let digits = format!("{remainder:0width$}", width = DECIMAL_SCALE as usize);
    let trimmed = digits.trim_end_matches('0');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// ── Serde helpers ─────────────────────────────────────────────────────

pub fn ser_u64_hex<S: serde::Serializer>(value: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{value:x}"))
}

pub fn ser_opt_u64_hex<S: serde::Serializer>(value: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(number) => s.serialize_str(&format!("0x{number:x}")),
        None => s.serialize_none(),
    }
}

/// Accept a chain quantity as either a hex string (the spec's form) or a JSON
/// number (what a hand-written client usually sends).
pub fn de_u64_flexible<'de, D>(de: D) -> Result<Option<u64>, D::Error>
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
    let parsed: Option<Either> = Option::deserialize(de)?;
    match parsed {
        None => Ok(None),
        Some(Either::Num(number)) => Ok(Some(number)),
        Some(Either::Hex(text)) => {
            let stripped = text
                .strip_prefix("0x")
                .or_else(|| text.strip_prefix("0X"))
                .unwrap_or(&text);
            u64::from_str_radix(stripped, 16)
                .map(Some)
                .map_err(|e| D::Error::custom(format!("invalid hex quantity {text:?}: {e}")))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::Attribute;

    fn select(json: serde_json::Value) -> Projection {
        let select: Select = serde_json::from_value(json).expect("valid select");
        Projection::resolve(Some(&select)).expect("resolvable")
    }

    fn entity() -> Entity {
        Entity {
            key: [0x11; 32],
            creator: [0x22; 20],
            owner: [0x33; 20],
            created_at_block: 0x191,
            last_modified_at_block: 0x1bb,
            expires_at: 0x2000,
            creation_flags: entity::CreationFlags::NONE,
            content_type: b"application/json".to_vec(),
            payload: vec![0x7b, 0x22],
            attributes: vec![
                Attribute::new(b"projectId".to_vec(), AttributeValue::Str("alice".into())),
                Attribute::new(b"version".to_vec(), AttributeValue::Int(4)),
                Attribute::new(
                    b"balance".to_vec(),
                    AttributeValue::u256_from_u64(1_000_000),
                ),
            ],
        }
    }

    #[test]
    fn the_default_projection_is_the_key_alone() {
        let json =
            serde_json::to_value(entity_data_from(entity(), &Projection::default())).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 1);
        assert!(json.get("key").is_some());
    }

    #[test]
    fn an_absent_select_resolves_to_the_default() {
        assert_eq!(Projection::resolve(None).unwrap(), Projection::default());
    }

    #[test]
    fn each_field_is_opt_in() {
        let projection = select(serde_json::json!({
            "key": true, "owner": true, "creator": true,
            "createdAt": true, "updatedAt": true, "expiresAt": true,
            "contentType": true, "payload": true,
        }));
        let json = serde_json::to_value(entity_data_from(entity(), &projection)).unwrap();
        assert_eq!(json["owner"], format!("0x{}", "33".repeat(20)));
        assert_eq!(json["creator"], format!("0x{}", "22".repeat(20)));
        assert_eq!(json["contentType"], "application/json");
        assert_eq!(json["payload"], "0x7b22");
        // Chain quantities are hex, like the eth_* namespace.
        assert_eq!(json["createdAt"], "0x191");
        assert_eq!(json["updatedAt"], "0x1bb");
        assert_eq!(json["expiresAt"], "0x2000");
        // Nothing was asked for beyond these.
        assert!(json.get("attributes").is_none());
        assert!(json.get("attributeSchema").is_none());
    }

    #[test]
    fn attribute_schema_carries_names_and_types_but_no_values() {
        let projection = select(serde_json::json!({ "attributeSchema": true }));
        let json = serde_json::to_value(entity_data_from(entity(), &projection)).unwrap();
        assert_eq!(
            json["attributeSchema"],
            serde_json::json!([
                { "name": "projectId", "type": "str" },
                { "name": "version", "type": "i32" },
                { "name": "balance", "type": "u256" },
            ])
        );
        assert!(json.get("attributes").is_none());
    }

    #[test]
    fn attributes_true_returns_all_of_them_typed() {
        let projection = select(serde_json::json!({ "attributes": true }));
        let json = serde_json::to_value(entity_data_from(entity(), &projection)).unwrap();
        assert_eq!(
            json["attributes"],
            serde_json::json!([
                { "name": "projectId", "type": "str", "value": "alice" },
                { "name": "version", "type": "i32", "value": 4 },
                { "name": "balance", "type": "u256", "value": "0xf4240" },
            ])
        );
    }

    #[test]
    fn a_named_subset_returns_only_those_attributes() {
        let projection = select(serde_json::json!({
            "attributes": { "version": true, "missing": true, "balance": false }
        }));
        let json = serde_json::to_value(entity_data_from(entity(), &projection)).unwrap();
        let names: Vec<_> = json["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["version"]);
    }

    /// The per-type JSON encodings the SDK decodes back into tagged values.
    #[test]
    fn values_encode_per_type() {
        let cases = [
            (AttributeValue::Bool(true), serde_json::json!(true)),
            (AttributeValue::Int(-42), serde_json::json!(-42)),
            (
                AttributeValue::u256_from_u64(1_000_000),
                serde_json::json!("0xf4240"),
            ),
            (AttributeValue::U256([0u8; 32]), serde_json::json!("0x0")),
            (
                AttributeValue::Str("hello".into()),
                serde_json::json!("hello"),
            ),
            (
                AttributeValue::EthereumAddress([0x11; 20]),
                serde_json::json!(format!("0x{}", "11".repeat(20))),
            ),
            (
                AttributeValue::EntityKey([0xab; 32]),
                serde_json::json!(format!("0x{}", "ab".repeat(32))),
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(attribute_value_json(&value), expected, "{value:?}");
        }
    }

    #[test]
    fn decimals_render_as_fixed_point_strings() {
        let scaled = |n: i128| {
            let raw =
                U256::from(n.unsigned_abs()) * U256::from(10u8).pow(U256::from(DECIMAL_SCALE));
            let word = if n < 0 {
                U256::ZERO.wrapping_sub(raw)
            } else {
                raw
            };
            AttributeValue::Decimal(word.to_be_bytes())
        };
        assert_eq!(attribute_value_json(&scaled(3)), serde_json::json!("3"));
        assert_eq!(attribute_value_json(&scaled(-3)), serde_json::json!("-3"));
        let half = U256::from(1_500_000_000_000_000_000u64);
        assert_eq!(
            attribute_value_json(&AttributeValue::Decimal(half.to_be_bytes())),
            serde_json::json!("1.5")
        );
        assert_eq!(
            attribute_value_json(&AttributeValue::Decimal(
                U256::ZERO.wrapping_sub(half).to_be_bytes()
            )),
            serde_json::json!("-1.5")
        );
        assert_eq!(
            attribute_value_json(&AttributeValue::Decimal([0u8; 32])),
            serde_json::json!("0")
        );
    }

    #[test]
    fn creation_flags_project_named_bits_and_the_raw_byte() {
        let mut e = entity();
        e.creation_flags =
            entity::CreationFlags::READONLY | entity::CreationFlags::PERMISSIONLESS_EXTENSION;

        let p = select(serde_json::json!({ "key": true, "creationFlags": true }));
        let flags = entity_data_from(e.clone(), &p).creation_flags.unwrap();
        assert!(flags.readonly && flags.permissionless_extension);
        assert_eq!(flags.raw, 0b11);

        let json = serde_json::to_value(flags).unwrap();
        assert_eq!(json["readonly"], true);
        assert_eq!(json["permissionlessExtension"], true);
        assert_eq!(json["raw"], 3);

        // A reserved bit this version cannot name still reaches the caller.
        e.creation_flags = entity::CreationFlags::from_stored_bits(0b1000_0000);
        let future = entity_data_from(e, &p).creation_flags.unwrap();
        assert!(!future.readonly && !future.permissionless_extension);
        assert_eq!(future.raw, 0b1000_0000);
    }

    /// Off by default, like every field but `key`.
    #[test]
    fn creation_flags_are_off_unless_selected() {
        let mut e = entity();
        e.creation_flags = entity::CreationFlags::READONLY;
        assert!(
            entity_data_from(e, &Projection::default())
                .creation_flags
                .is_none()
        );
    }

    #[test]
    fn an_unknown_select_field_is_rejected() {
        let bad = serde_json::from_value::<Select>(serde_json::json!({ "payloadd": true }));
        assert!(
            bad.is_err(),
            "a typo'd projection must not silently do nothing"
        );
    }

    /// The cursor binds to these bytes, so any change a caller can make to
    /// `select` has to change them.
    #[test]
    fn fingerprints_distinguish_projections() {
        let base = Projection::default().fingerprint();
        assert_ne!(
            base,
            select(serde_json::json!({ "key": true, "owner": true })).fingerprint()
        );
        assert_ne!(
            select(serde_json::json!({ "attributes": true })).fingerprint(),
            select(serde_json::json!({ "attributes": { "a": true } })).fingerprint(),
        );
        assert_ne!(
            select(serde_json::json!({ "attributes": { "a": true } })).fingerprint(),
            select(serde_json::json!({ "attributes": { "b": true } })).fingerprint(),
        );
        // Equal projections fingerprint equally, whatever order the map came in.
        assert_eq!(
            select(serde_json::json!({ "attributes": { "a": true, "b": true } })).fingerprint(),
            select(serde_json::json!({ "attributes": { "b": true, "a": true } })).fingerprint(),
        );
    }
}
