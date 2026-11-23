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

mod finalize;
mod reader;
mod run;
#[cfg(test)]
mod testkit;
mod writer;
pub use finalize::{Finalized, finalize, set_prefilled_root};
pub use reader::{Entities, MAX_LINE_BYTES};
pub use run::{Prefill, Ran, run};
pub use writer::{Written, write_archive};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u64,
    parts: Vec<PathBuf>,
}

/// An archive: its index file and the parts it names, in order.
///
/// The index is read once, by [`Archive::open`]: the part list and the bytes
/// that go into the hash come from that one read, so an index edited later
/// cannot pair a new index with the old part list.
#[derive(Debug, Clone)]
pub struct Archive {
    index: PathBuf,
    index_bytes: Vec<u8>,
    parts: Vec<PathBuf>,
}

impl Archive {
    /// Open the archive whose index file is `index`. Part paths are relative
    /// to the index's directory and must stay inside it, so the archive is one
    /// directory that can be copied whole: an absolute path, or one with `..`,
    /// is refused.
    pub fn open(index: impl Into<PathBuf>) -> Result<Self> {
        let index = index.into();
        let index_bytes =
            std::fs::read(&index).wrap_err_with(|| format!("open {}", index.display()))?;
        let parsed: Index = serde_json::from_slice(&index_bytes)
            .wrap_err_with(|| format!("parse {}", index.display()))?;
        ensure!(
            parsed.version == 1,
            "{}: unsupported archive version {}",
            index.display(),
            parsed.version
        );
        for part in &parsed.parts {
            ensure!(
                !part.as_os_str().is_empty()
                    && part
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "{}: part {} must be a path inside the archive's directory, with no `..`",
                index.display(),
                part.display()
            );
        }
        let root = index.parent().unwrap_or(Path::new("."));
        let parts = parsed.parts.iter().map(|part| root.join(part)).collect();
        Ok(Self {
            index,
            index_bytes,
            parts,
        })
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
    /// part once, in fixed-size chunks; the index's bytes are the ones
    /// [`Archive::open`] read.
    ///
    /// A writer importing the archive does not call this: reading the entities
    /// computes the same hash from the very bytes it imports. This is for
    /// checking an archive without importing it, as a resume does for the last
    /// one committed.
    pub fn hash(&self) -> Result<B256> {
        let mut hasher = self.hasher();
        for path in &self.parts {
            hash_file(&mut hasher, path).wrap_err_with(|| format!("hash {}", path.display()))?;
        }
        Ok(hasher.finalize())
    }

    /// A hasher that has taken in the index, ready for the parts.
    pub(crate) fn hasher(&self) -> Keccak256 {
        let mut hasher = Keccak256::new();
        hasher.update((self.index_bytes.len() as u64).to_be_bytes());
        hasher.update(&self.index_bytes);
        hasher
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

    /// The hash of one fixed archive, worked out by hand from the formula in
    /// the crate docs (lengths as big-endian `u64`, then the bytes, index
    /// first) and hashed outside this crate. A change to the formula, such as
    /// leaving the index out, fails here even though every other test compares
    /// two hashes of this code's own making.
    #[test]
    fn a_fixed_archive_has_a_fixed_hash() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.json");
        std::fs::write(&index, r#"{"version":1,"parts":["a.jsonl","b.jsonl"]}"#).unwrap();
        for (name, body) in PARTS {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        assert_eq!(
            Archive::open(index).unwrap().hash().unwrap(),
            "0xebc0fa9a977f9350127f74d29b815476b556422e46fa20f46fceaf7f6805b5d7"
                .parse::<B256>()
                .unwrap()
        );
    }

    /// The index is read once: editing it after `open` changes neither the
    /// part list nor the hash, which both describe what was opened.
    #[test]
    fn the_index_is_read_once() {
        let (dir, archive) = archive(&PARTS, &["a.jsonl", "b.jsonl"]);
        let before = archive.hash().unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            serde_json::json!({ "version": 1, "parts": ["a.jsonl"] }).to_string(),
        )
        .unwrap();
        assert_eq!(archive.parts().len(), 2);
        assert_eq!(archive.hash().unwrap(), before);
    }

    /// Parts stay inside the archive's directory, so the archive can be
    /// copied whole.
    #[test]
    fn a_part_outside_the_archive_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.json");
        for part in ["/etc/hosts", "../other/a.jsonl", "sub/../../a.jsonl", ""] {
            std::fs::write(
                &index,
                serde_json::json!({ "version": 1, "parts": [part] }).to_string(),
            )
            .unwrap();
            let error = Archive::open(&index).unwrap_err().to_string();
            assert!(
                error.contains("inside the archive's directory"),
                "{part}: {error}"
            );
        }
        // A subdirectory is fine.
        std::fs::write(
            &index,
            serde_json::json!({ "version": 1, "parts": ["sub/a.jsonl"] }).to_string(),
        )
        .unwrap();
        assert!(Archive::open(&index).is_ok());
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
