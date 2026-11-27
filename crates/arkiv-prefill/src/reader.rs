//! The streaming archive reader: each entity line becomes its creator and the
//! executor ops that write it.
//!
//! Only the format is checked here, plus the structural rules the calldata
//! decoder would have applied (attribute names, by the decoder's own checks;
//! count; no zero transfer target), since prefill skips the decoder. What the
//! executor checks itself (key collisions, permanent expiry) and the store's
//! size limits are left to them.
//!
//! Reading also hashes: [`Entities::hash`] is the archive hash of exactly the
//! bytes the entities came from.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use alloy_primitives::{Address, B256, Keccak256};
use arkiv_bindings::types::{Ident32ByteError, validate_ident32_bytes};
use arkiv_interfaces::constants::{MAX_BYTES_VALUE_LEN, MAX_STR_BYTES, OP_MAX_ATTRIBUTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags, annotations};
use arkiv_interfaces::execution::Op;
use arkiv_interfaces::primitives::UserAddress;
use base64::{Engine, engine::general_purpose::STANDARD};
use eyre::Result;
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

/// The longest line a part may have, in bytes.
///
/// Comfortably above the largest legal entity line: a payload at
/// `MAX_BYTES_VALUE_LEN` (128 KiB) is about 171 KiB of base64, and
/// `OP_MAX_ATTRIBUTES` attributes of at most `MAX_STR_BYTES` each add a few KiB
/// even fully escaped. The limit is enforced while reading, so a part without
/// line breaks fails at this many bytes instead of being read whole into
/// memory.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// An iterator over an archive's entities, in order: per line, the creator and
/// its ops. Holds one open part and one line at a time.
///
/// It also hashes what it reads: once every line has been read,
/// [`Entities::hash`] is the archive hash of the very bytes the entities came
/// from, so a writer never tags a commit with the hash of a file that changed
/// between hashing and importing it, and reads the archive once instead of
/// twice.
pub struct Entities<'a> {
    parts: std::slice::Iter<'a, std::path::PathBuf>,
    current: Option<Part<'a>>,
    buf: Vec<u8>,
    /// `None` once an error ended the iteration.
    hasher: Option<Keccak256>,
    hash: Option<B256>,
}

/// The part being read.
struct Part<'a> {
    path: &'a Path,
    reader: BufReader<File>,
    line: usize,
    /// Its length when opened, which the hash commits to, and the bytes read
    /// so far, which must reach it exactly.
    len: u64,
    read: u64,
}

impl Archive {
    /// The archive's entities, read line by line. An error names `file:line`
    /// and, for a bad field, the field; the iterator ends after it.
    pub fn entities(&self) -> Entities<'_> {
        Entities {
            parts: self.parts().iter(),
            current: None,
            buf: Vec::new(),
            hasher: Some(self.hasher()),
            hash: None,
        }
    }
}

impl Entities<'_> {
    /// The archive hash of everything read, once the last line has been: the
    /// same value as [`Archive::hash`]. `None` before that, and after an error.
    pub fn hash(&self) -> Option<B256> {
        self.hash
    }

    /// Where the last line read is, as `file:line`, for an error about what
    /// that line does rather than how it is written: the executor refusing it.
    pub fn at(&self) -> Option<String> {
        let part = self.current.as_ref()?;
        Some(format!("{}:{}", part.path.display(), part.line))
    }

    /// End the iteration: an archive with a bad line or part is never read past it.
    fn fail(&mut self, error: eyre::Report) -> Option<Result<(UserAddress, Vec<Op>)>> {
        self.parts = [].iter();
        self.current = None;
        self.hasher = None;
        Some(Err(error))
    }
}

impl Iterator for Entities<'_> {
    type Item = Result<(UserAddress, Vec<Op>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let hasher = self.hasher.as_mut()?;
            if self.current.is_none() {
                let Some(path) = self.parts.next() else {
                    self.hash = self.hasher.take().map(Keccak256::finalize);
                    return None;
                };
                // Stop at a part that cannot be read: skipping it would hand
                // the writer a different archive.
                let opened = File::open(path).and_then(|file| Ok((file.metadata()?.len(), file)));
                let (len, file) = match opened {
                    Ok(opened) => opened,
                    Err(e) => {
                        return self.fail(eyre::eyre!("open {}: {e}", path.display()));
                    }
                };
                hasher.update(len.to_be_bytes());
                self.current = Some(Part {
                    path,
                    reader: BufReader::new(file),
                    line: 0,
                    len,
                    read: 0,
                });
            }
            let part = self.current.as_mut().expect("opened above");
            self.buf.clear();
            let read = (&mut part.reader)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut self.buf);
            part.line += 1;
            let at = format!("{}:{}", part.path.display(), part.line);
            let n = match read {
                Ok(n) => n,
                Err(e) => return self.fail(eyre::eyre!("{at}: {e}")),
            };
            if n == 0 {
                if part.read != part.len {
                    let path = part.path.display().to_string();
                    return self.fail(eyre::eyre!("{path}: changed while it was read"));
                }
                self.current = None;
                continue;
            }
            if n > MAX_LINE_BYTES {
                return self.fail(eyre::eyre!(
                    "{at}: line longer than {MAX_LINE_BYTES} bytes; is the part missing its line breaks?"
                ));
            }
            hasher.update(&self.buf);
            part.read += n as u64;
            let line = match std::str::from_utf8(&self.buf) {
                Ok(line) => line,
                Err(e) => return self.fail(eyre::eyre!("{at}: not UTF-8: {e}")),
            };
            if line.trim().is_empty() {
                continue;
            }
            return match parse_line(line) {
                Ok(entity) => Some(Ok(entity)),
                Err(e) => self.fail(eyre::eyre!("{at}: {e}")),
            };
        }
    }
}

/// A line's fields, refusing a key given twice: a JSON object may repeat one,
/// and a plain map would keep the last copy without a word, so two readers of
/// the same line could disagree on what it says.
struct Fields(BTreeMap<String, Value>);

impl<'de> serde::Deserialize<'de> for Fields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Fields;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Fields, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some(name) = map.next_key::<String>()? {
                    if fields.contains_key(&name) {
                        return Err(serde::de::Error::custom(format!("{name:?} is given twice")));
                    }
                    let value = map.next_value()?;
                    fields.insert(name, value);
                }
                Ok(Fields(fields))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

/// One line's creator and ops, or what is wrong with it.
fn parse_line(line: &str) -> Result<(UserAddress, Vec<Op>), String> {
    let Fields(fields) = serde_json::from_str(line).map_err(|e| {
        if e.is_data() {
            e.to_string()
        } else {
            format!("not a JSON object: {e}")
        }
    })?;
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
    // Prefilled entities exist at block 0, so one expiring at 0 would never be
    // live. There is no upper limit: `max_lifetime` limits transactions, not
    // prefilled data.
    if expires_at == 0 {
        return Err("\"$expiresAt\" is 0: the entity would already be expired at block 0".into());
    }

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
        check_length("$contentType", &value)?;
        attributes.push(Attribute::unindexed("contentType", value));
    }
    if fields.contains_key("$payload") {
        let payload = STANDARD
            .decode(text("$payload")?)
            .map_err(|e| format!("\"$payload\" is not base64: {e}"))?;
        check_length("$payload", &AttributeValue::Bytes(payload.clone()))?;
        attributes.push(Attribute::unindexed(
            "payload",
            AttributeValue::Bytes(payload),
        ));
    }
    for (name, value) in fields.iter().filter(|(name, _)| !name.starts_with('$')) {
        check_attribute_name(name)?;
        let value = parse_attribute(name, value)?;
        check_length(name, &value)?;
        attributes.push(Attribute::new(name.as_bytes(), value));
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

/// The name rules a transaction's attributes meet, checked by the decoder's
/// own functions so the two cannot drift: not engine-controlled (no `$`), and a
/// valid `Ident32` once padded to its 32 bytes.
fn check_attribute_name(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    if annotations::is_engine_controlled(bytes) {
        return Err(format!("{name:?}: the engine owns names starting with $"));
    }
    let mut padded = [0u8; 32];
    if bytes.len() > padded.len() {
        return Err(format!("{name:?}: attribute names are at most 32 bytes"));
    }
    padded[..bytes.len()].copy_from_slice(bytes);
    validate_ident32_bytes(&padded).map_err(|e| match e {
        Ident32ByteError::Empty => format!("{name:?}: an attribute name cannot be empty"),
        Ident32ByteError::InvalidByte { position, value } => {
            format!("{name:?}: invalid attribute name byte 0x{value:02x} at position {position}")
        }
    })
}

/// The length limits a transaction's values meet, from the same constants as
/// the decoder's: a longer value would otherwise get as far as the store and
/// be refused there without its name.
fn check_length(name: &str, value: &AttributeValue) -> Result<(), String> {
    let (length, max) = match value {
        AttributeValue::Str(text) => (text.len(), MAX_STR_BYTES),
        AttributeValue::Bytes(bytes) => (bytes.len(), MAX_BYTES_VALUE_LEN),
        _ => return Ok(()),
    };
    if length > max {
        return Err(format!("{name:?} is {length} bytes long, at most {max}"));
    }
    Ok(())
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
                serde_json::json!({ "$expiresAt": 0 }),
                r#""$expiresAt" is 0: the entity would already be expired at block 0"#,
            ),
            (
                serde_json::json!({ "$expiresAt": "0" }),
                r#""$expiresAt" is 0: the entity would already be expired at block 0"#,
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
                serde_json::json!({ "kind": format!("str:{}", "x".repeat(MAX_STR_BYTES + 1)) }),
                r#""kind" is 129 bytes long, at most 128"#,
            ),
            (
                serde_json::json!({ "$contentType": "x".repeat(MAX_STR_BYTES + 1) }),
                r#""$contentType" is 129 bytes long, at most 128"#,
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

    /// Reading the entities computes the archive hash from the bytes read: the
    /// same value as hashing the archive on its own, and only once the last
    /// line is in.
    #[test]
    fn reading_hashes_what_it_reads() {
        let (_dir, archive) = archive(&[line(serde_json::json!({})), String::new()]);
        let mut entities = archive.entities();
        assert!(entities.next().unwrap().is_ok());
        assert_eq!(entities.hash(), None, "not before the end");
        assert!(entities.next().is_none());
        assert_eq!(entities.hash(), Some(archive.hash().unwrap()));
    }

    #[test]
    fn an_archive_with_a_bad_line_has_no_hash() {
        let (_dir, archive) = archive(&["{".into()]);
        let mut entities = archive.entities();
        assert!(entities.next().unwrap().is_err());
        assert!(entities.next().is_none());
        assert_eq!(entities.hash(), None);
    }

    /// A part without line breaks fails at the line limit, instead of being
    /// read whole into memory first.
    #[test]
    fn a_line_past_the_limit_is_refused() {
        let (_dir, archive) = archive(&["x".repeat(MAX_LINE_BYTES + 10)]);
        let error = archive.entities().next().unwrap().unwrap_err().to_string();
        assert!(error.ends_with(&format!("p.jsonl:1: line longer than {MAX_LINE_BYTES} bytes; is the part missing its line breaks?")), "{error}");
    }

    /// The largest line a legal entity can produce fits well under the limit:
    /// the biggest payload and every attribute at its longest, all escaped.
    #[test]
    fn the_largest_legal_line_fits_the_limit() {
        use arkiv_interfaces::constants::{MAX_BYTES_VALUE_LEN, MAX_STR_BYTES};
        let mut extra = serde_json::Map::new();
        extra.insert(
            "$payload".into(),
            STANDARD.encode(vec![0xff; MAX_BYTES_VALUE_LEN]).into(),
        );
        for i in 0..OP_MAX_ATTRIBUTES - 1 {
            let name = format!("a{i:0>31}");
            extra.insert(
                name,
                format!("str:{}", "\u{1}".repeat(MAX_STR_BYTES)).into(),
            );
        }
        let text = line(Value::Object(extra));
        assert!(text.len() * 2 < MAX_LINE_BYTES, "{} bytes", text.len());
        let (_dir, archive) = archive(&[text]);
        assert!(archive.entities().next().unwrap().is_ok());
    }

    /// A key given twice in one line is refused, rather than its last copy
    /// winning without a word.
    #[test]
    fn a_key_given_twice_is_refused() {
        let base = line(serde_json::json!({}));
        for (twice, name) in [
            (r#""$expiresAt":"5000000""#, "$expiresAt"),
            (r#""kind":"str:a","kind":"str:b""#, "kind"),
        ] {
            let text = format!("{},{twice}}}", &base[..base.len() - 1]);
            let (_dir, archive) = archive(std::slice::from_ref(&text));
            let error = archive.entities().next().unwrap().unwrap_err().to_string();
            assert!(
                error.contains(&format!("{name:?} is given twice")),
                "{text}: {error}"
            );
        }
    }

    /// Names are checked by the decoder's own rules. A byte above 127 is
    /// refused, where `Ident32::encode` panics on it (debug) or takes it for
    /// another letter (release).
    #[test]
    fn attribute_names_follow_the_decoders_rules() {
        for (name, expected) in [
            ("Énergie", "invalid attribute name byte 0xc3 at position 0"),
            // A byte after a NUL, which would give the name two spellings.
            ("ok\u{0}x", "invalid attribute name byte 0x78 at position 3"),
            (&"a".repeat(33) as &str, "at most 32 bytes"),
        ] {
            let mut extra = serde_json::Map::new();
            extra.insert(name.into(), "str:x".into());
            let (_dir, archive) = archive(&[line(Value::Object(extra))]);
            let error = archive.entities().next().unwrap().unwrap_err().to_string();
            assert!(error.contains(expected), "{name:?}: {error}");
        }
    }
}
