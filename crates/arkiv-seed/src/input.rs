//! Explicit-key seed input: the subset of draft seed input v1 the Sourcify
//! export writes.
//!
//! A `seed.json` index names ordered JSON Lines parts; each non-empty line is
//! one entity with its key, identities, expiry, payload and typed attributes
//! spelled out. Nothing is minted: every key is given, so the creators'
//! entity nonces stay where they were.
//!
//! ```json
//! {"version": 1, "chainId": 7706815, "entities": [{"path": "entities/0001.jsonl"}]}
//! ```
//!
//! ```json
//! {"$key":"0x11…","$creator":"0x22…","$owner":"0x22…","$expiresAt":"never",
//!  "$contentType":"text/plain","$payload":"aGk=","$creationFlags":[],
//!  "kind":"str:sourcefile","size":"i32:2"}
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256};
use arkiv_bindings::Ident32;
use arkiv_interfaces::constants::{ENTITY_MAX_ATTRIBUTES, MAX_PAYLOAD_BYTES, MAX_STR_BYTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags, Entity};
use base64::{Engine, engine::general_purpose::STANDARD};
use eyre::{Result, WrapErr, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::SeedEntity;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Index {
    version: u64,
    chain_id: u64,
    entities: Vec<Part>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    path: PathBuf,
}

/// The system fields an entity line may carry.
const SYSTEM_FIELDS: [&str; 7] = [
    "$key",
    "$creator",
    "$owner",
    "$expiresAt",
    "$contentType",
    "$payload",
    "$creationFlags",
];

/// A streaming reader over a seed index's entity parts, in order.
pub struct Input {
    paths: Vec<PathBuf>,
    part: usize,
    reader: Option<BufReader<File>>,
    line: usize,
    /// The chain id the index names. Explicit keys do not depend on it; it is
    /// the chain the export came from.
    pub chain_id: u64,
}

impl Input {
    /// Open `index`, a `seed.json`. Part paths are relative to it.
    pub fn open(index: &Path) -> Result<Self> {
        let parsed: Index = serde_json::from_reader(
            File::open(index).wrap_err_with(|| format!("open {}", index.display()))?,
        )
        .wrap_err_with(|| format!("parse {}", index.display()))?;
        ensure!(
            parsed.version == 1,
            "unsupported seed version {}",
            parsed.version
        );
        let root = index.parent().unwrap_or(Path::new("."));
        Ok(Self {
            paths: parsed
                .entities
                .into_iter()
                .map(|part| root.join(part.path))
                .collect(),
            part: 0,
            reader: None,
            line: 0,
            chain_id: parsed.chain_id,
        })
    }

    /// The next entity, or `None` past the last part. An error names the
    /// part and line.
    pub fn next_entity(&mut self) -> Result<Option<SeedEntity>> {
        loop {
            if self.part == self.paths.len() {
                return Ok(None);
            }
            let path = &self.paths[self.part];
            let reader = match &mut self.reader {
                Some(reader) => reader,
                None => {
                    self.line = 0;
                    self.reader.insert(BufReader::new(
                        File::open(path).wrap_err_with(|| format!("open {}", path.display()))?,
                    ))
                }
            };
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                self.reader = None;
                self.part += 1;
                continue;
            }
            self.line += 1;
            if line.trim().is_empty() {
                continue;
            }
            return parse_entity(&line)
                .map(Some)
                .wrap_err_with(|| format!("{}:{}", path.display(), self.line));
        }
    }
}

/// How many entities `index` holds: its parts' non-empty lines, counted
/// without parsing them. What a progress report needs before the real pass.
pub fn count_entities(index: &Path) -> Result<u64> {
    let input = Input::open(index)?;
    let mut count = 0;
    for path in &input.paths {
        let reader =
            BufReader::new(File::open(path).wrap_err_with(|| format!("open {}", path.display()))?);
        for line in reader.split(b'\n') {
            if line?.iter().any(|byte| !byte.is_ascii_whitespace()) {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn parse_entity(line: &str) -> Result<SeedEntity> {
    let fields: BTreeMap<String, Value> = serde_json::from_str(line)?;
    let text = |name: &str| -> Result<&str> {
        fields
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| eyre::eyre!("missing or non-string field {name}"))
    };
    let key: B256 = text("$key")?.parse().wrap_err("$key")?;
    let creator: Address = text("$creator")?.parse().wrap_err("$creator")?;
    let owner: Address = text("$owner")?.parse().wrap_err("$owner")?;
    ensure!(
        creator != Address::ZERO,
        "the zero address cannot create entities"
    );
    ensure!(
        owner == creator,
        "this importer requires $owner = $creator (a transfer at genesis is not supported)"
    );
    let expires_at = match fields.get("$expiresAt") {
        None => u64::MAX,
        Some(Value::String(s)) if s == "never" => u64::MAX,
        Some(Value::String(s)) => s.parse().wrap_err("$expiresAt")?,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| eyre::eyre!("invalid $expiresAt {v}"))?,
    };
    ensure!(expires_at > 0, "$expiresAt must be a future block");
    let content_type = text("$contentType")?.as_bytes().to_vec();
    let payload = STANDARD.decode(text("$payload")?).wrap_err("$payload")?;
    ensure!(
        payload.len() <= MAX_PAYLOAD_BYTES,
        "payload of {} bytes exceeds {MAX_PAYLOAD_BYTES}",
        payload.len()
    );
    let mut bits = 0;
    if let Some(flags) = fields.get("$creationFlags") {
        for flag in flags
            .as_array()
            .ok_or_else(|| eyre::eyre!("$creationFlags must be an array"))?
        {
            bits |= match flag.as_str() {
                Some("readonly") => 1,
                Some("permissionlessExtension") => 2,
                _ => bail!("unknown creation flag {flag}"),
            };
        }
    }

    let mut attributes = Vec::new();
    for (name, value) in &fields {
        if name.starts_with('$') {
            ensure!(
                SYSTEM_FIELDS.contains(&name.as_str()),
                "unsupported system field {name}"
            );
            continue;
        }
        Ident32::encode(name).map_err(|e| eyre::eyre!("invalid attribute name {name}: {e}"))?;
        let (tag, value) = value
            .as_str()
            .and_then(|s| s.split_once(':'))
            .ok_or_else(|| eyre::eyre!("attribute {name} must be a tagged string"))?;
        let value = match tag {
            "str" => {
                ensure!(
                    value.len() <= MAX_STR_BYTES,
                    "attribute {name}: string longer than {MAX_STR_BYTES} bytes"
                );
                AttributeValue::Str(value.into())
            }
            "i32" => AttributeValue::Int(value.parse()?),
            "u64" => AttributeValue::U64(match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16)?,
                None => value.parse()?,
            }),
            "bool" => AttributeValue::Bool(match value {
                "true" => true,
                "false" => false,
                _ => bail!("attribute {name}: invalid bool {value:?}"),
            }),
            "addr" => AttributeValue::EthereumAddress(value.parse::<Address>()?.into_array()),
            "key" => AttributeValue::EntityKey(value.parse::<B256>()?.into()),
            "bytes32" => AttributeValue::Bytes32(value.parse::<B256>()?.into()),
            _ => bail!("attribute {name}: unsupported tag {tag}"),
        };
        // The map iterates by name, so the list is already in the canonical,
        // ascending order the node's decoder enforces.
        attributes.push(Attribute::new(name.as_bytes().to_vec(), value));
    }
    ensure!(
        attributes.len() <= ENTITY_MAX_ATTRIBUTES,
        "{} attributes exceed {ENTITY_MAX_ATTRIBUTES}",
        attributes.len()
    );

    let creator = creator.into_array();
    Ok(SeedEntity {
        entity: Entity {
            key: key.0,
            creator,
            owner: creator,
            created_at_block: 0,
            last_modified_at_block: 0,
            expires_at,
            creation_flags: CreationFlags::from_bits(bits)
                .ok_or_else(|| eyre::eyre!("invalid creation flags {bits}"))?,
            content_type,
            payload,
            attributes,
        },
        minted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(extra: serde_json::Value) -> String {
        let mut value = serde_json::json!({
            "$key": B256::repeat_byte(2),
            "$creator": Address::repeat_byte(1), "$owner": Address::repeat_byte(1),
            "$expiresAt": "never", "$contentType": "text/plain", "$payload": "aGVsbG8=",
            "$creationFlags": ["readonly"],
        });
        for (name, field) in extra.as_object().unwrap() {
            value[name] = field.clone();
        }
        value.to_string()
    }

    #[test]
    fn a_line_becomes_an_explicit_key_genesis_entity() {
        let seeded = parse_entity(&line(serde_json::json!({
            "kind": "str:code", "size": "i32:-2", "n": "u64:0x10",
            "ok": "bool:true", "at": format!("addr:{}", Address::repeat_byte(3)),
        })))
        .unwrap();
        assert!(!seeded.minted, "an explicit key mints nothing");
        let entity = seeded.entity;
        assert_eq!(entity.key, [2; 32]);
        assert_eq!(entity.expires_at, u64::MAX);
        assert_eq!(entity.payload, b"hello");
        assert!(entity.creation_flags.is_readonly());
        assert_eq!(entity.created_at_block, 0);
        let names: Vec<_> = entity.attributes.iter().map(|a| a.key.clone()).collect();
        assert_eq!(names, [&b"at"[..], b"kind", b"n", b"ok", b"size"]);
        assert_eq!(entity.attributes[2].value, AttributeValue::U64(16));
    }

    #[test]
    fn bytes32_rejects_incorrect_width_and_non_hex_values() {
        for value in [
            "0x01".to_owned(),
            format!("0x{}", "11".repeat(33)),
            format!("0x{}", "zz".repeat(32)),
        ] {
            let bad = line(serde_json::json!({ "hash": format!("bytes32:{value}") }));
            assert!(
                parse_entity(&bad).is_err(),
                "accepted invalid bytes32: {value}"
            );
        }
    }

    #[test]
    fn untagged_and_unknown_fields_are_refused() {
        for extra in [
            serde_json::json!({ "kind": "code" }),
            serde_json::json!({ "kind": 3 }),
            serde_json::json!({ "$createdAt": "1" }),
            serde_json::json!({ "$owner": Address::repeat_byte(9) }),
        ] {
            assert!(parse_entity(&line(extra.clone())).is_err(), "{extra}");
        }
    }
}
