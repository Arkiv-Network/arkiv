//! Chain prefill input: the archive format, the archive hash and the
//! streaming reader ([`Archive::entities`]).
//!
//! # Archives
//!
//! Prefill data comes as ordered archives A0, A1, … Each archive becomes one
//! GolemDB commit, so an archive's size is what bounds the writer's memory.
//!
//! An archive is an **index file** naming its **parts**, in order:
//!
//! ```text
//! A1/index.json
//! A1/entities-0001.jsonl
//! A1/entities-0002.jsonl
//! ```
//!
//! ```json
//! {"version": 1, "parts": ["entities-0001.jsonl", "entities-0002.jsonl"]}
//! ```
//!
//! `version` is `1`; no other field is allowed. Part paths are relative to the
//! index file's directory. The entities are read in index order, part by part,
//! line by line.
//!
//! # Parts
//!
//! A part is JSON Lines: one entity per line, one JSON object per entity. Blank
//! lines are skipped. A `$` field is a system field; every other field is a
//! user attribute.
//!
//! ```json
//! {"$key":"0x9c1e…","$creator":"0x2222…","$owner":"0x3333…","$expiresAt":"1500000","$contentType":"text/plain","$payload":"aGk=","$creationFlags":["readonly"],"kind":"str:sourcefile","size":"i32:2"}
//! ```
//!
//! | field            | required | value                                                         |
//! |------------------|----------|---------------------------------------------------------------|
//! | `$key`           | yes      | the entity key, 32 bytes hex                                  |
//! | `$creator`       | yes      | the creating account, 20 bytes hex                            |
//! | `$owner`         | yes      | the owner; when it is not `$creator` the entity is transferred |
//! | `$expiresAt`     | yes      | the expiry block, a decimal number or string; not 0           |
//! | `$contentType`   | no       | a string, stored as the unindexed `str` attribute `contentType` |
//! | `$payload`       | no       | base64, stored as the unindexed `bytes` attribute `payload`   |
//! | `$creationFlags` | no       | an array of `"readonly"`, `"permissionlessExtension"`         |
//!
//! `$contentType` and `$payload` follow the convention `arkiv-cli` writes: the
//! protocol has neither, so they are ordinary user attributes.
//!
//! A user attribute is an indexed attribute written as a tagged string,
//! `"<tag>:<value>"`:
//!
//! | tag       | value                          | example                  |
//! |-----------|--------------------------------|--------------------------|
//! | `str`     | the rest of the string         | `"str:sourcefile"`       |
//! | `i32`     | a decimal `i32`                | `"i32:-2"`               |
//! | `u64`     | a decimal or `0x` hex `u64`    | `"u64:0x10"`             |
//! | `bool`    | `true` or `false`              | `"bool:true"`            |
//! | `addr`    | 20 bytes hex                   | `"addr:0x3333…"`         |
//! | `key`     | an entity key, 32 bytes hex    | `"key:0x9c1e…"`          |
//! | `bytes32` | 32 bytes hex                   | `"bytes32:0xab…"`        |
//!
//! # Archive hash
//!
//! An archive's hash tags its commit, so a resumed prefill can tell which
//! archives are done and catch a changed or reordered archive list. It is the
//! keccak256 of, in order:
//!
//! 1. the index file's length as a big-endian `u64`, then its bytes;
//! 2. for each part in index order, its length as a big-endian `u64`, then its
//!    bytes.
//!
//! The bytes are the files' raw bytes, not their parsed contents, so any
//! change to any file (even whitespace) changes the hash. The length prefixes
//! pin where each file ends, so moving a line from one part to the next
//! changes it too.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use alloy_primitives::{B256, Keccak256};
use eyre::{Result, WrapErr, ensure};
use serde::Deserialize;

mod reader;
pub use reader::Entities;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u64,
    parts: Vec<PathBuf>,
}

/// An archive: its index file and the parts it names, in order.
#[derive(Debug, Clone)]
pub struct Archive {
    index: PathBuf,
    parts: Vec<PathBuf>,
}

impl Archive {
    /// Open the archive whose index file is `index`.
    pub fn open(index: impl Into<PathBuf>) -> Result<Self> {
        let index = index.into();
        let parsed: Index = serde_json::from_reader(
            File::open(&index).wrap_err_with(|| format!("open {}", index.display()))?,
        )
        .wrap_err_with(|| format!("parse {}", index.display()))?;
        ensure!(
            parsed.version == 1,
            "{}: unsupported archive version {}",
            index.display(),
            parsed.version
        );
        let root = index.parent().unwrap_or(Path::new("."));
        let parts = parsed.parts.iter().map(|part| root.join(part)).collect();
        Ok(Self { index, parts })
    }

    /// The index file.
    pub fn index(&self) -> &Path {
        &self.index
    }

    /// The parts, in order.
    pub fn parts(&self) -> &[PathBuf] {
        &self.parts
    }

    /// The archive hash: see the [crate docs](crate#archive-hash). Reads every
    /// file once, in fixed-size chunks.
    pub fn hash(&self) -> Result<B256> {
        let mut hasher = Keccak256::new();
        for path in std::iter::once(&self.index).chain(&self.parts) {
            hash_file(&mut hasher, path).wrap_err_with(|| format!("hash {}", path.display()))?;
        }
        Ok(hasher.finalize())
    }
}

fn hash_file(hasher: &mut Keccak256, path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    hasher.update(len.to_be_bytes());
    let mut buf = vec![0; 64 * 1024];
    let mut read = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }
    ensure!(read == len, "changed while hashing");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive in a fresh directory: `parts` written as named, the index
    /// listing `order`.
    fn archive(parts: &[(&str, &str)], order: &[&str]) -> (tempfile::TempDir, Archive) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in parts {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let index = dir.path().join("index.json");
        std::fs::write(
            &index,
            serde_json::json!({ "version": 1, "parts": order }).to_string(),
        )
        .unwrap();
        let archive = Archive::open(index).unwrap();
        (dir, archive)
    }

    const PARTS: [(&str, &str); 2] = [
        ("a.jsonl", "{\"x\":1}\n{\"y\":2}\n"),
        ("b.jsonl", "{\"z\":3}\n"),
    ];

    fn hash(parts: &[(&str, &str)], order: &[&str]) -> B256 {
        archive(parts, order).1.hash().unwrap()
    }

    #[test]
    fn same_files_give_the_same_hash() {
        assert_eq!(
            hash(&PARTS, &["a.jsonl", "b.jsonl"]),
            hash(&PARTS, &["a.jsonl", "b.jsonl"])
        );
    }

    #[test]
    fn any_change_changes_the_hash() {
        let base = hash(&PARTS, &["a.jsonl", "b.jsonl"]);
        let changed = [
            // One byte.
            hash(
                &[PARTS[0], ("b.jsonl", "{\"z\":4}\n")],
                &["a.jsonl", "b.jsonl"],
            ),
            // Reordered parts.
            hash(&PARTS, &["b.jsonl", "a.jsonl"]),
            // A dropped part.
            hash(&PARTS, &["a.jsonl"]),
            // A line moved across a part boundary: same bytes, different split.
            hash(
                &[
                    ("a.jsonl", "{\"x\":1}\n"),
                    ("b.jsonl", "{\"y\":2}\n{\"z\":3}\n"),
                ],
                &["a.jsonl", "b.jsonl"],
            ),
        ];
        for (i, other) in changed.iter().enumerate() {
            assert_ne!(base, *other, "change {i} kept the hash");
        }
    }

    #[test]
    fn parts_resolve_next_to_the_index() {
        let (dir, archive) = archive(&PARTS, &["a.jsonl", "b.jsonl"]);
        assert_eq!(
            archive.parts(),
            [dir.path().join("a.jsonl"), dir.path().join("b.jsonl")]
        );
    }

    #[test]
    fn a_bad_index_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.json");
        for body in [
            r#"{"version": 2, "parts": []}"#,
            r#"{"version": 1, "parts": [], "chainId": 1}"#,
            r#"{"parts": []}"#,
        ] {
            std::fs::write(&index, body).unwrap();
            assert!(Archive::open(&index).is_err(), "{body}");
        }
    }
}
