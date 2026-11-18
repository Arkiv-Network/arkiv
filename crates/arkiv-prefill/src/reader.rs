//! The streaming archive reader: each entity line becomes its creator and the
//! executor ops that write it.
//!
//! Only the format is checked here, plus the structural rules the calldata
//! decoder would have applied (attribute names, count, no zero transfer
//! target), since prefill skips the decoder. Everything the executor checks
//! itself (key collisions, the expiry window) and the store's size limits are
//! left to them.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use alloy_primitives::{Address, B256};
use arkiv_bindings::Ident32;
use arkiv_interfaces::constants::OP_MAX_ATTRIBUTES;
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags};
use arkiv_interfaces::execution::Op;
use arkiv_interfaces::primitives::UserAddress;
use base64::{Engine, engine::general_purpose::STANDARD};
use eyre::{Result, WrapErr};
use serde_json::Value;

use crate::Archive;

/// The system fields a line may carry.
const SYSTEM_FIELDS: [&str; 7] = [
    "$key",
    "$creator",
    "$owner",
    "$expiresAt",
    "$contentType",
    "$payload",
    "$creationFlags",
];

/// An iterator over an archive's entities, in order: per line, the creator and
/// its ops. Holds one open part and one line at a time.
pub struct Entities<'a> {
    parts: std::slice::Iter<'a, std::path::PathBuf>,
    current: Option<(&'a Path, BufReader<File>, usize)>,
    buf: String,
}

impl Archive {
    /// The archive's entities, read line by line. An error names `file:line`
    /// and, for a bad field, the field; the iterator ends after it.
    pub fn entities(&self) -> Entities<'_> {
        Entities {
            parts: self.parts().iter(),
            current: None,
            buf: String::new(),
        }
    }
}

impl Iterator for Entities<'_> {
    type Item = Result<(UserAddress, Vec<Op>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (path, reader, line) = match &mut self.current {
                Some(current) => current,
                None => {
                    let path = self.parts.next()?;
                    let file = match File::open(path) {
                        Ok(file) => file,
                        Err(e) => {
                            // Stop here: skipping a part would hand the writer
                            // a different archive.
                            self.fuse();
                            return Some(
                                Err(e).wrap_err_with(|| format!("open {}", path.display())),
                            );
                        }
                    };
                    self.current.insert((path, BufReader::new(file), 0))
                }
            };
            self.buf.clear();
            let read = reader.read_line(&mut self.buf);
            *line += 1;
            let at = format!("{}:{line}", path.display());
            match read {
                Ok(0) => self.current = None,
                Ok(_) if self.buf.trim().is_empty() => {}
                Ok(_) => {
                    let parsed = parse_line(&self.buf).map_err(|e| eyre::eyre!("{at}: {e}"));
                    if parsed.is_err() {
                        self.fuse();
                    }
                    return Some(parsed);
                }
                Err(e) => {
                    self.fuse();
                    return Some(Err(e).wrap_err(at));
                }
            }
        }
    }
}

impl Entities<'_> {
    /// End the iteration: an archive with a bad line or part is never read past it.
    fn fuse(&mut self) {
        self.parts = [].iter();
        self.current = None;
    }
}

/// One line's creator and ops, or what is wrong with it.
fn parse_line(line: &str) -> Result<(UserAddress, Vec<Op>), String> {
    let fields: BTreeMap<String, Value> =
        serde_json::from_str(line).map_err(|e| format!("not a JSON object: {e}"))?;
    for name in fields.keys().filter(|name| name.starts_with('$')) {
        if !SYSTEM_FIELDS.contains(&name.as_str()) {
            return Err(format!("unknown field {name:?}"));
        }
    }
    let text = |name: &str| match fields.get(name) {
        None => Err(format!("missing field {name:?}")),
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(v) => Err(format!("{name:?} is not a string: {v}")),
    };
    let key: B256 = parse(text("$key")?, "$key", "an entity key")?;
    let creator: Address = parse(text("$creator")?, "$creator", "an address")?;
    let owner: Address = parse(text("$owner")?, "$owner", "an address")?;
    if creator == Address::ZERO {
        return Err("\"$creator\" is the zero address".into());
    }
    if owner == Address::ZERO {
        return Err("\"$owner\" is the zero address".into());
    }
    let expires_at = match fields.get("$expiresAt") {
        None => return Err("missing field \"$expiresAt\"".into()),
        Some(Value::String(s)) => parse(s, "$expiresAt", "a number")?,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("\"$expiresAt\" is not a number: {v}"))?,
    };

    let mut flags = CreationFlags::NONE.bits();
    if let Some(value) = fields.get("$creationFlags") {
        let list = value
            .as_array()
            .ok_or_else(|| format!("\"$creationFlags\" is not an array: {value}"))?;
        for flag in list {
            flags |= match flag.as_str() {
                Some("readonly") => CreationFlags::READONLY.bits(),
                Some("permissionlessExtension") => CreationFlags::PERMISSIONLESS_EXTENSION.bits(),
                _ => return Err(format!("\"$creationFlags\": unknown flag {flag}")),
            };
        }
    }

    let mut attributes = Vec::new();
    if fields.contains_key("$contentType") {
        let value = AttributeValue::Str(text("$contentType")?.into());
        attributes.push(Attribute::unindexed("contentType", value));
    }
    if fields.contains_key("$payload") {
        let payload = STANDARD
            .decode(text("$payload")?)
            .map_err(|e| format!("\"$payload\" is not base64: {e}"))?;
        attributes.push(Attribute::unindexed(
            "payload",
            AttributeValue::Bytes(payload),
        ));
    }
    for (name, value) in fields.iter().filter(|(name, _)| !name.starts_with('$')) {
        Ident32::encode(name).map_err(|e| format!("{name:?}: invalid attribute name: {e}"))?;
        attributes.push(Attribute::new(
            name.as_bytes(),
            parse_attribute(name, value)?,
        ));
    }
    // Ascending by name, as the decoder requires of a create.
    attributes.sort_by(|a, b| a.key.cmp(&b.key));
    if let Some(pair) = attributes.windows(2).find(|w| w[0].key == w[1].key) {
        let name = String::from_utf8_lossy(&pair[0].key);
        return Err(format!("{name:?} is given twice"));
    }
    if attributes.len() > OP_MAX_ATTRIBUTES {
        return Err(format!(
            "{} attributes, at most {OP_MAX_ATTRIBUTES}",
            attributes.len()
        ));
    }

    let key = key.0;
    let mut ops = vec![Op::Create {
        key,
        expires_at,
        // Only defined bits can be set above.
        creation_flags: CreationFlags::from_stored_bits(flags),
        attributes,
    }];
    if owner != creator {
        ops.push(Op::Transfer {
            key,
            new_owner: owner.into_array(),
        });
    }
    Ok((creator.into_array(), ops))
}

/// A user attribute: `"<tag>:<value>"`.
fn parse_attribute(name: &str, value: &Value) -> Result<AttributeValue, String> {
    let (tag, value) = value
        .as_str()
        .and_then(|s| s.split_once(':'))
        .ok_or_else(|| format!("{name:?} is not a tagged string: {value}"))?;
    Ok(match tag {
        "str" => AttributeValue::Str(value.into()),
        "i32" => AttributeValue::Int(parse(value, name, "an i32")?),
        "u64" => AttributeValue::U64(match value.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).map_err(|_| not(name, "a u64", value))?,
            None => parse(value, name, "a u64")?,
        }),
        "bool" => AttributeValue::Bool(parse(value, name, "a bool")?),
        "addr" => AttributeValue::EthereumAddress(
            parse::<Address>(value, name, "an address")?.into_array(),
        ),
        "key" => AttributeValue::EntityKey(parse::<B256>(value, name, "an entity key")?.0),
        "bytes32" => AttributeValue::Bytes32(parse::<B256>(value, name, "32 bytes")?.0),
        _ => return Err(format!("{name:?}: unknown tag {tag:?}")),
    })
}

fn parse<T: std::str::FromStr>(value: &str, field: &str, what: &str) -> Result<T, String> {
    value.parse().map_err(|_| not(field, what, value))
}

fn not(field: &str, what: &str, value: &str) -> String {
    format!("{field:?} is not {what}: {value:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0x0202020202020202020202020202020202020202020202020202020202020202";
    const ALICE: &str = "0x1111111111111111111111111111111111111111";
    const BOB: &str = "0x2222222222222222222222222222222222222222";

    /// A valid line, with `extra` fields set (or, as `null`, removed).
    fn line(extra: Value) -> String {
        let mut value = serde_json::json!({
            "$key": KEY, "$creator": ALICE, "$owner": ALICE, "$expiresAt": "1500000",
        });
        for (name, field) in extra.as_object().unwrap() {
            match field {
                Value::Null => value.as_object_mut().unwrap().remove(name),
                _ => value
                    .as_object_mut()
                    .unwrap()
                    .insert(name.clone(), field.clone()),
            };
        }
        value.to_string()
    }

    /// An archive of one part holding `lines`.
    fn archive(lines: &[String]) -> (tempfile::TempDir, Archive) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.jsonl"), lines.join("\n")).unwrap();
        let index = dir.path().join("index.json");
        std::fs::write(&index, r#"{"version":1,"parts":["p.jsonl"]}"#).unwrap();
        let archive = Archive::open(index).unwrap();
        (dir, archive)
    }

    #[test]
    fn a_line_becomes_a_create() {
        let (_dir, archive) = archive(&[line(serde_json::json!({
            "$contentType": "text/plain", "$payload": "aGk=",
            "$creationFlags": ["readonly", "permissionlessExtension"],
            "kind": "str:code", "size": "i32:-2", "n": "u64:0x10", "ok": "bool:true",
            "at": format!("addr:{BOB}"), "ref": format!("key:{KEY}"), "h": format!("bytes32:{KEY}"),
        }))]);
        let all: Vec<_> = archive.entities().collect::<Result<_>>().unwrap();
        let [(creator, ops)] = &all[..] else {
            panic!("{all:?}")
        };
        assert_eq!(*creator, [0x11; 20]);
        let expected = Op::Create {
            key: [2; 32],
            expires_at: 1_500_000,
            creation_flags: CreationFlags::from_bits(3).unwrap(),
            attributes: vec![
                Attribute::new("at", AttributeValue::EthereumAddress([0x22; 20])),
                Attribute::unindexed("contentType", AttributeValue::Str("text/plain".into())),
                Attribute::new("h", AttributeValue::Bytes32([2; 32])),
                Attribute::new("kind", AttributeValue::Str("code".into())),
                Attribute::new("n", AttributeValue::U64(16)),
                Attribute::new("ok", AttributeValue::Bool(true)),
                Attribute::unindexed("payload", AttributeValue::Bytes(b"hi".to_vec())),
                Attribute::new("ref", AttributeValue::EntityKey([2; 32])),
                Attribute::new("size", AttributeValue::Int(-2)),
            ],
        };
        assert_eq!(ops, &[expected]);
    }

    #[test]
    fn another_owner_adds_a_transfer_and_blank_lines_are_skipped() {
        let (_dir, archive) = archive(&[
            String::new(),
            line(serde_json::json!({ "$owner": BOB, "$expiresAt": 7 })),
            "  ".into(),
        ]);
        let all: Vec<_> = archive.entities().collect::<Result<_>>().unwrap();
        let [(creator, ops)] = &all[..] else {
            panic!("{all:?}")
        };
        assert_eq!(*creator, [0x11; 20]);
        assert!(matches!(ops[0], Op::Create { expires_at: 7, .. }));
        assert_eq!(
            ops[1],
            Op::Transfer {
                key: [2; 32],
                new_owner: [0x22; 20]
            }
        );
    }

    #[test]
    fn a_bad_line_names_its_place_and_field() {
        let zero = "0x0000000000000000000000000000000000000000";
        for (extra, expected) in [
            (
                serde_json::json!({ "$expiresAt": "soon" }),
                r#""$expiresAt" is not a number: "soon""#,
            ),
            (
                serde_json::json!({ "$expiresAt": null }),
                r#"missing field "$expiresAt""#,
            ),
            (
                serde_json::json!({ "$colour": "red" }),
                r#"unknown field "$colour""#,
            ),
            (
                serde_json::json!({ "$key": "0x01" }),
                r#""$key" is not an entity key"#,
            ),
            (
                serde_json::json!({ "$creator": 5 }),
                r#""$creator" is not a string"#,
            ),
            (
                serde_json::json!({ "$owner": zero }),
                r#""$owner" is the zero address"#,
            ),
            (
                serde_json::json!({ "$payload": "!!" }),
                r#""$payload" is not base64"#,
            ),
            (
                serde_json::json!({ "$creationFlags": ["x"] }),
                r#""$creationFlags": unknown flag"#,
            ),
            (
                serde_json::json!({ "kind": "code" }),
                r#""kind" is not a tagged string"#,
            ),
            (
                serde_json::json!({ "kind": "f64:1" }),
                r#""kind": unknown tag "f64""#,
            ),
            (
                serde_json::json!({ "size": "i32:big" }),
                r#""size" is not an i32: "big""#,
            ),
            (
                serde_json::json!({ "9x": "str:a" }),
                r#""9x": invalid attribute name"#,
            ),
            (
                serde_json::json!({ "payload": "str:a", "$payload": "" }),
                r#""payload" is given twice"#,
            ),
        ] {
            // The bad line is line 2, after a good one.
            let (dir, archive) = archive(&[line(serde_json::json!({})), line(extra)]);
            let mut entities = archive.entities();
            assert!(entities.next().unwrap().is_ok());
            let err = entities.next().unwrap().unwrap_err().to_string();
            let at = format!("{}:2: ", dir.path().join("p.jsonl").display());
            assert!(err.starts_with(&at) && err.contains(expected), "{err}");
            assert!(entities.next().is_none(), "the reader stops at an error");
        }
    }

    #[test]
    fn too_many_attributes_are_refused() {
        let extra: serde_json::Map<_, _> = (0..=OP_MAX_ATTRIBUTES)
            .map(|i| (format!("a{i}"), Value::from("bool:true")))
            .collect();
        let (_dir, archive) = archive(&[line(extra.into())]);
        let err = archive.entities().next().unwrap().unwrap_err().to_string();
        assert!(err.contains("33 attributes, at most 32"), "{err}");
    }

    #[test]
    fn parts_are_read_in_index_order() {
        let dir = tempfile::tempdir().unwrap();
        for (part, key) in [("a.jsonl", 1u8), ("b.jsonl", 2)] {
            let key = B256::repeat_byte(key).to_string();
            std::fs::write(
                dir.path().join(part),
                line(serde_json::json!({ "$key": key })),
            )
            .unwrap();
        }
        let index = dir.path().join("index.json");
        std::fs::write(&index, r#"{"version":1,"parts":["b.jsonl","a.jsonl"]}"#).unwrap();
        let keys: Vec<_> = Archive::open(index)
            .unwrap()
            .entities()
            .map(|entry| match &entry.unwrap().1[0] {
                Op::Create { key, .. } => key[0],
                op => panic!("{op:?}"),
            })
            .collect();
        assert_eq!(keys, [2, 1]);
    }
}
