//! The Valkey key layout.
//!
//! Every key this crate touches is built here, so the layout can be read in
//! one place and changed without hunting format strings. Each store instance
//! owns a **namespace** prefix, which is what lets several — a devnet node and
//! a test, or two parallel tests — share one server without collision.
//!
//! ```text
//! {ns}:head                 the canonical head, as a decimal integer
//! {ns}:digest               the committed state digest, 32 raw bytes
//! {ns}:records              SET of hex record keys present at head
//! {ns}:rec:{key}            HASH cell name -> tag ‖ value, plus "#v" -> version
//! {ns}:commit:{id}:undo     LIST of encoded changes that produced this commit
//! {ns}:commit:{id}:hash     that commit's digest, 32 raw bytes
//! {ns}:commit:{id}:tag      that commit's host tag, 32 raw bytes, if tagged
//! {ns}:tag:{tag}            reverse index: host tag -> commit id
//! {ns}:branches             SET of live branch ids
//! {ns}:branch:{id}:meta     HASH origin/version/depth
//! {ns}:branch:{id}:diff     HASH hex record key -> encoded record or tombstone
//! {ns}:nextbranch           the branch-id counter, never rewound
//! ```
//!
//! Record keys are hex rather than raw bytes because they appear inside key
//! names and hash fields, where a binary-safe but human-illegible encoding
//! buys nothing.

use arkiv_interfaces::store::RecordKey;

/// The hash field holding a record's version, inside `{ns}:rec:{key}`.
///
/// `#` is reserved for store-internal meta entries and rejected in any
/// caller-supplied cell map, so this can never collide with a real cell.
pub(crate) const VERSION_FIELD: &str = "#v";

/// One store instance's key prefix.
#[derive(Debug, Clone)]
pub(crate) struct Namespace(String);

impl Namespace {
    pub(crate) fn new(prefix: impl Into<String>) -> Self {
        Self(prefix.into())
    }

    /// Every key under this namespace — the pattern a teardown scans for.
    pub(crate) fn wildcard(&self) -> String {
        format!("{}:*", self.0)
    }

    pub(crate) fn head(&self) -> String {
        format!("{}:head", self.0)
    }

    pub(crate) fn digest(&self) -> String {
        format!("{}:digest", self.0)
    }

    pub(crate) fn records(&self) -> String {
        format!("{}:records", self.0)
    }

    pub(crate) fn record(&self, key: RecordKey) -> String {
        format!("{}:rec:{}", self.0, hex(&key.0))
    }

    pub(crate) fn commit_undo(&self, commit: u64) -> String {
        format!("{}:commit:{commit}:undo", self.0)
    }

    pub(crate) fn commit_hash(&self, commit: u64) -> String {
        format!("{}:commit:{commit}:hash", self.0)
    }

    pub(crate) fn commit_tag(&self, commit: u64) -> String {
        format!("{}:commit:{commit}:tag", self.0)
    }

    pub(crate) fn tag_index(&self, tag: [u8; 32]) -> String {
        format!("{}:tag:{}", self.0, hex(&tag))
    }

    pub(crate) fn branches(&self) -> String {
        format!("{}:branches", self.0)
    }

    pub(crate) fn branch_meta(&self, branch: u64) -> String {
        format!("{}:branch:{branch}:meta", self.0)
    }

    pub(crate) fn branch_diff(&self, branch: u64) -> String {
        format!("{}:branch:{branch}:diff", self.0)
    }

    pub(crate) fn next_branch(&self) -> String {
        format!("{}:nextbranch", self.0)
    }
}

/// Lowercase hex, for the parts of a key a human may have to read.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble is a hex digit"));
        out.push(char::from_digit((byte & 0x0F) as u32, 16).expect("nibble is a hex digit"));
    }
    out
}

/// The inverse of [`hex`] for a record key, rejecting anything that is not
/// exactly 32 bytes of hex.
pub(crate) fn record_key_from_hex(text: &str) -> Option<RecordKey> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (slot, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        *slot = ((high << 4) | low) as u8;
    }
    Some(RecordKey(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_round_trip_through_hex() {
        let key = RecordKey([0xDE; 32]);
        assert_eq!(record_key_from_hex(&hex(&key.0)), Some(key));
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert_eq!(record_key_from_hex("tooshort"), None);
        assert_eq!(record_key_from_hex(&"zz".repeat(32)), None);
    }

    #[test]
    fn namespaces_do_not_collide() {
        let first = Namespace::new("a");
        let second = Namespace::new("b");
        assert_ne!(first.head(), second.head());
        assert!(first.record(RecordKey([0; 32])).starts_with("a:"));
    }
}
