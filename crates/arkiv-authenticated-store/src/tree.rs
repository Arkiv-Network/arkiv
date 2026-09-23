//! Persistent ordered Merkle treap. Key-derived priorities make the tree shape
//! canonical for a given map, regardless of insertion order. Keys remain plain
//! ordered bytes; hashes identify records, not search keys. Expected logarithmic
//! height, not a worst-case balancing guarantee for adversarially chosen keys.

use crate::storage::RecordStore;
use alloy_primitives::{B256, keccak256};
use eyre::{Result, ensure, eyre};
use std::{
    collections::BTreeMap,
    ops::{Bound, ControlFlow},
};

pub const EMPTY_ROOT: B256 = B256::ZERO;
const NODE: &[u8] = b"arkiv.tree.node.v1";
const BLOB: &[u8] = b"arkiv.tree.blob.v1";
const PRIORITY: &[u8] = b"arkiv.tree.priority.v1";

#[derive(Clone)]
struct Node {
    key: Vec<u8>,
    value: B256,
    left: B256,
    right: B256,
}

impl Node {
    fn encode(&self) -> Vec<u8> {
        // All fields after the key have a fixed width, so no length delimiter
        // is needed and there is exactly one encoding for each node.
        let mut bytes = NODE.to_vec();
        bytes.extend_from_slice(&self.key);
        bytes.extend_from_slice(self.value.as_slice());
        bytes.extend_from_slice(self.left.as_slice());
        bytes.extend_from_slice(self.right.as_slice());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.starts_with(NODE) && bytes.len() >= NODE.len() + 96,
            "invalid tree node"
        );
        let tail = bytes.len() - 96;
        Ok(Self {
            key: bytes[NODE.len()..tail].to_vec(),
            value: B256::from_slice(&bytes[tail..tail + 32]),
            left: B256::from_slice(&bytes[tail + 32..tail + 64]),
            right: B256::from_slice(&bytes[tail + 64..]),
        })
    }

    fn priority(&self) -> (B256, &[u8]) {
        let mut bytes = PRIORITY.to_vec();
        bytes.extend_from_slice(&self.key);
        (keccak256(bytes), &self.key)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub nodes_read: usize,
    pub values_read: usize,
}

/// Portable genesis state. Records are authenticated by `root`, which must also
/// be present in the genesis system account. This is not a new canonical head.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub root: B256,
    pub records: BTreeMap<B256, alloy_primitives::Bytes>,
}

impl Snapshot {
    /// Validate the complete graph and canonical tree before writing any records.
    pub fn import<S: RecordStore>(&self, store: S) -> Result<()> {
        let memory = crate::MemoryStore::default();
        let records: BTreeMap<_, _> = self.records.iter().map(|(h, b)| (*h, b.to_vec())).collect();
        for (hash, bytes) in &records {
            ensure!(keccak256(bytes) == *hash, "invalid snapshot record {hash}");
        }
        memory.write(&records)?;
        let source = Tree::open(memory.clone(), self.root)?;
        let reachable = source.snapshot()?;
        ensure!(
            reachable.records == self.records,
            "snapshot contains unreachable records"
        );
        let mut canonical = Tree::open(crate::MemoryStore::default(), EMPTY_ROOT)?;
        for bytes in records.values().filter(|b| b.starts_with(NODE)) {
            let node = Node::decode(bytes)?;
            canonical.insert(node.key, source.value(node.value)?)?;
        }
        ensure!(canonical.root() == self.root, "noncanonical snapshot tree");
        store.write(&records)
    }
}

/// An isolated mutable view. Dropping it discards unpersisted writes. Cloning
/// creates a branch; immutable records already on disk are shared.
#[derive(Clone)]
pub struct Tree<S: RecordStore> {
    store: S,
    root: B256,
    pending: BTreeMap<B256, Vec<u8>>,
    transaction_depth: usize,
}

impl<S: RecordStore> Tree<S> {
    pub fn open(store: S, root: B256) -> Result<Self> {
        let tree = Self {
            store,
            root,
            pending: BTreeMap::new(),
            transaction_depth: 0,
        };
        if root != EMPTY_ROOT {
            tree.node(root)?;
        }
        Ok(tree)
    }

    pub fn root(&self) -> B256 {
        self.root
    }

    pub(crate) fn checkpoint(&mut self) -> B256 {
        self.transaction_depth += 1;
        self.root
    }

    pub(crate) fn finish_checkpoint(&mut self, root: B256, success: bool) {
        self.transaction_depth -= 1;
        if !success {
            self.root = root;
        }
    }

    fn record(&self, hash: B256) -> Result<Vec<u8>> {
        let bytes = match self.pending.get(&hash) {
            Some(bytes) => bytes.clone(),
            None => self
                .store
                .get(hash)?
                .ok_or_else(|| eyre!("missing authenticated record {hash}"))?,
        };
        ensure!(
            keccak256(&bytes) == hash,
            "corrupt authenticated record {hash}"
        );
        Ok(bytes)
    }

    fn node(&self, hash: B256) -> Result<Node> {
        Node::decode(&self.record(hash)?)
    }

    fn value(&self, hash: B256) -> Result<Vec<u8>> {
        let bytes = self.record(hash)?;
        ensure!(bytes.starts_with(BLOB), "invalid value record");
        Ok(bytes[BLOB.len()..].to_vec())
    }

    fn put_record(&mut self, bytes: Vec<u8>) -> B256 {
        let hash = keccak256(&bytes);
        self.pending.insert(hash, bytes);
        hash
    }

    fn put_node(&mut self, node: &Node) -> B256 {
        self.put_record(node.encode())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut root = self.root;
        while root != EMPTY_ROOT {
            let node = self.node(root)?;
            match key.cmp(&node.key) {
                std::cmp::Ordering::Equal => return self.value(node.value).map(Some),
                std::cmp::Ordering::Less => root = node.left,
                std::cmp::Ordering::Greater => root = node.right,
            }
        }
        Ok(None)
    }

    pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        let mut bytes = BLOB.to_vec();
        bytes.extend_from_slice(&value);
        let value = self.put_record(bytes);
        self.root = self.insert_at(self.root, &key, value)?;
        Ok(())
    }

    fn insert_at(&mut self, root: B256, key: &[u8], value: B256) -> Result<B256> {
        if root == EMPTY_ROOT {
            return Ok(self.put_node(&Node {
                key: key.to_vec(),
                value,
                left: EMPTY_ROOT,
                right: EMPTY_ROOT,
            }));
        }
        let mut node = self.node(root)?;
        match key.cmp(&node.key) {
            std::cmp::Ordering::Equal => node.value = value,
            std::cmp::Ordering::Less => {
                node.left = self.insert_at(node.left, key, value)?;
                let mut child = self.node(node.left)?;
                if child.priority() < node.priority() {
                    node.left = child.right;
                    child.right = self.put_node(&node);
                    return Ok(self.put_node(&child));
                }
            }
            std::cmp::Ordering::Greater => {
                node.right = self.insert_at(node.right, key, value)?;
                let mut child = self.node(node.right)?;
                if child.priority() < node.priority() {
                    node.right = child.left;
                    child.left = self.put_node(&node);
                    return Ok(self.put_node(&child));
                }
            }
        }
        Ok(self.put_node(&node))
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<()> {
        self.root = self.remove_at(self.root, key)?;
        Ok(())
    }

    fn remove_at(&mut self, root: B256, key: &[u8]) -> Result<B256> {
        if root == EMPTY_ROOT {
            return Ok(root);
        }
        let mut node = self.node(root)?;
        match key.cmp(&node.key) {
            std::cmp::Ordering::Equal => return self.merge(node.left, node.right),
            std::cmp::Ordering::Less => node.left = self.remove_at(node.left, key)?,
            std::cmp::Ordering::Greater => node.right = self.remove_at(node.right, key)?,
        }
        Ok(self.put_node(&node))
    }

    fn merge(&mut self, left: B256, right: B256) -> Result<B256> {
        if left == EMPTY_ROOT {
            return Ok(right);
        }
        if right == EMPTY_ROOT {
            return Ok(left);
        }
        let mut l = self.node(left)?;
        let mut r = self.node(right)?;
        if l.priority() < r.priority() {
            l.right = self.merge(l.right, right)?;
            Ok(self.put_node(&l))
        } else {
            r.left = self.merge(left, r.left)?;
            Ok(self.put_node(&r))
        }
    }

    /// Bounded, ascending traversal. Values outside the bounds are never read;
    /// unrelated subtrees are skipped. The callback can stop without materializing
    /// the remainder. Reversed bounds yield an empty scan.
    pub fn scan(
        &self,
        low: Bound<&[u8]>,
        high: Bound<&[u8]>,
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<ControlFlow<()>>,
    ) -> Result<ScanStats> {
        let mut stats = ScanStats::default();
        let mut stack = Vec::new();
        let mut next = self.root;
        loop {
            while next != EMPTY_ROOT {
                let node = self.node(next)?;
                stats.nodes_read += 1;
                let below = match low {
                    Bound::Included(k) => node.key.as_slice() < k,
                    Bound::Excluded(k) => node.key.as_slice() <= k,
                    Bound::Unbounded => false,
                };
                let above = match high {
                    Bound::Included(k) => node.key.as_slice() > k,
                    Bound::Excluded(k) => node.key.as_slice() >= k,
                    Bound::Unbounded => false,
                };
                if below {
                    next = node.right;
                } else if above {
                    next = node.left;
                } else {
                    next = node.left;
                    stack.push(node);
                }
            }
            let Some(node) = stack.pop() else {
                break;
            };
            let value = self.value(node.value)?;
            stats.values_read += 1;
            if visit(&node.key, &value)?.is_break() {
                break;
            }
            next = node.right;
        }
        Ok(stats)
    }

    /// Export only records reachable from this root, verifying every hash.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let mut records = BTreeMap::new();
        let mut todo = vec![self.root];
        while let Some(hash) = todo.pop() {
            if hash == EMPTY_ROOT || records.contains_key(&hash) {
                continue;
            }
            let bytes = self.record(hash)?;
            let node = Node::decode(&bytes)?;
            records.insert(hash, bytes.into());
            self.value(node.value)?;
            records.insert(node.value, self.record(node.value)?.into());
            todo.extend([node.left, node.right]);
        }
        Ok(Snapshot {
            root: self.root,
            records,
        })
    }

    /// Flush immutable records before the caller publishes this root in Ethereum
    /// state. Old roots remain readable. Does not select a canonical branch.
    pub fn persist(&mut self) -> Result<B256> {
        ensure!(
            self.transaction_depth == 0,
            "cannot persist inside an open transaction"
        );
        // Only persist records reachable from the final root. Intermediate
        // versions made while editing a batch need not occupy disk forever.
        let mut reachable = BTreeMap::new();
        let mut todo = vec![self.root];
        while let Some(hash) = todo.pop() {
            if hash == EMPTY_ROOT || reachable.contains_key(&hash) {
                continue;
            }
            let Some(bytes) = self.pending.get(&hash) else {
                continue;
            };
            let node = Node::decode(bytes)?;
            reachable.insert(hash, bytes.clone());
            if let Some(value) = self.pending.get(&node.value) {
                reachable.insert(node.value, value.clone());
            }
            todo.extend([node.left, node.right]);
        }
        if !reachable.is_empty() {
            self.store.write(&reachable)?;
        }
        self.pending.clear();
        Ok(self.root)
    }

    /// Atomic logical changes, without copying the accumulated record buffer.
    /// On error newly allocated records become unreachable; they cannot affect
    /// any reads or commitments. Garbage collection is outside this prototype.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let before = self.checkpoint();
        let result = f(self);
        self.finish_checkpoint(before, result.is_ok());
        result
    }
}
