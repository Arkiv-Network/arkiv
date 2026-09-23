//! The trie operations: lookup, batch update by path copying, ordered
//! iteration. All of them are free functions on [`Trie`] over a root hash and
//! a node store; there is no trie object, because a root *is* the trie.

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_trie::nodes::{BranchNode, ExtensionNode, LeafNode, RlpNode, TrieNode};
use alloy_trie::{EMPTY_ROOT_HASH, Nibbles, TrieMask};
use core::cmp::Ordering;
use std::collections::BTreeMap;

use crate::store::{NodeReader, NodeSink};

#[derive(Debug)]
pub enum TrieError<E> {
    Store(E),
    /// A node the trie refers to is not in the store.
    MissingNode(B256),
    Decode(alloy_rlp::Error),
    /// The key is a prefix of another key in the trie.
    PrefixKey(Vec<u8>),
}

/// A batch of changes: `Some` inserts or replaces, `None` deletes. A `BTreeMap`
/// so the keys are unique and sorted.
pub type Changes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// One `(key, value)` entry of a walk.
pub type Item<E> = Result<(Vec<u8>, Vec<u8>), TrieError<E>>;

type Entry = (Nibbles, Option<Vec<u8>>);

/// The operations. See the module docs.
pub struct Trie;

impl Trie {
    /// The value under `key` in the trie at `root`.
    pub fn get<S: NodeReader>(
        store: &S,
        root: B256,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TrieError<S::Error>> {
        let key = Nibbles::unpack(key);
        let mut depth = 0;
        let mut node = match load_root(store, root)? {
            Some(n) => n,
            None => return Ok(None),
        };
        loop {
            match node {
                TrieNode::EmptyRoot => return Ok(None),
                TrieNode::Leaf(leaf) => {
                    return Ok((leaf.key == key.slice(depth..)).then_some(leaf.value));
                }
                TrieNode::Extension(ext) => {
                    if !key.slice(depth..).starts_with(&ext.key) {
                        return Ok(None);
                    }
                    depth += ext.key.len();
                    node = load(store, &ext.child)?;
                }
                TrieNode::Branch(branch) => {
                    let Some(nibble) = key.get(depth) else {
                        return Ok(None);
                    };
                    let Some(child) = child_at(&branch, nibble) else {
                        return Ok(None);
                    };
                    depth += 1;
                    node = load(store, child)?;
                }
            }
        }
    }

    /// Apply `changes` to the trie at `root`, writing every new node into
    /// `store`, and return the new root. The trie at `root` is untouched.
    pub fn update<S: NodeReader + NodeSink>(
        store: &mut S,
        root: B256,
        changes: &Changes,
    ) -> Result<B256, TrieError<S::Error>> {
        if changes.is_empty() {
            return Ok(root);
        }
        let entries: Vec<Entry> = changes
            .iter()
            .map(|(k, v)| (Nibbles::unpack(k), v.clone()))
            .collect();
        let old = load_root(store, root)?;
        match update_node(store, old, 0, &entries)? {
            None => Ok(EMPTY_ROOT_HASH),
            Some(node) => Ok(put_root(store, &node)),
        }
    }

    /// The entries of the trie at `root` with key `>= from`, in key order.
    pub fn iter_from<'a, S: NodeReader>(
        store: &'a S,
        root: B256,
        from: &[u8],
    ) -> Result<TrieIter<'a, S>, TrieError<S::Error>> {
        let mut stack = Vec::new();
        if let Some(node) = load_root(store, root)? {
            stack.push(Frame::Node(Nibbles::new(), node));
        }
        Ok(TrieIter {
            store,
            lower: Nibbles::unpack(from),
            stack,
        })
    }

    /// The entries with `from <= key < to`, in key order. `to = None` is
    /// unbounded.
    pub fn range<'a, S: NodeReader>(
        store: &'a S,
        root: B256,
        from: &[u8],
        to: Option<&[u8]>,
    ) -> Result<impl Iterator<Item = Item<S::Error>> + 'a, TrieError<S::Error>> {
        let to = to.map(|t| t.to_vec());
        let iter = Self::iter_from(store, root, from)?;
        Ok(iter.take_while(move |item| match (item, &to) {
            (Ok((k, _)), Some(to)) => k < to,
            _ => true,
        }))
    }
}

// ---------------------------------------------------------------------------
// Loading and storing
// ---------------------------------------------------------------------------

fn decode<E>(mut rlp: &[u8]) -> Result<TrieNode, TrieError<E>> {
    TrieNode::decode(&mut rlp).map_err(TrieError::Decode)
}

fn load_root<S: NodeReader>(
    store: &S,
    root: B256,
) -> Result<Option<TrieNode>, TrieError<S::Error>> {
    if root == EMPTY_ROOT_HASH {
        return Ok(None);
    }
    let rlp = store
        .node(&root)
        .map_err(TrieError::Store)?
        .ok_or(TrieError::MissingNode(root))?;
    decode(&rlp).map(Some)
}

/// Resolve a child reference: a hash is looked up, a short node is inline.
fn load<S: NodeReader>(store: &S, r: &RlpNode) -> Result<TrieNode, TrieError<S::Error>> {
    match r.as_hash() {
        Some(hash) => {
            let rlp = store
                .node(&hash)
                .map_err(TrieError::Store)?
                .ok_or(TrieError::MissingNode(hash))?;
            decode(&rlp)
        }
        None => decode(r.as_slice()),
    }
}

/// Encode `node` and store it if it is hash-referenced; return the reference
/// a parent embeds.
fn put<S: NodeSink>(store: &mut S, node: &TrieNode) -> RlpNode {
    let mut rlp = Vec::new();
    let r = node.rlp(&mut rlp);
    if let Some(hash) = r.as_hash() {
        store.put_node(hash, rlp);
    }
    r
}

/// The root is always referenced by hash, even when its encoding is short.
fn put_root<S: NodeSink>(store: &mut S, node: &TrieNode) -> B256 {
    let mut rlp = Vec::new();
    node.encode(&mut rlp);
    let hash = keccak256(&rlp);
    store.put_node(hash, rlp);
    hash
}

fn child_at(branch: &BranchNode, nibble: u8) -> Option<&RlpNode> {
    if !branch.state_mask.is_bit_set(nibble) {
        return None;
    }
    let below = (branch.state_mask.get() & ((1u16 << nibble) - 1)).count_ones() as usize;
    branch
        .stack
        .get(branch.as_ref().first_child_index() + below)
}

/// A branch's children as a 16-slot table.
fn children_of(branch: &BranchNode) -> [Option<RlpNode>; 16] {
    let mut out: [Option<RlpNode>; 16] = Default::default();
    for (nibble, child) in branch.as_ref().children() {
        out[nibble as usize] = child.cloned();
    }
    out
}

fn branch_from(children: [Option<RlpNode>; 16]) -> BranchNode {
    let mut mask = TrieMask::default();
    let mut stack = Vec::new();
    for (i, child) in children.into_iter().enumerate() {
        if let Some(c) = child {
            mask.set_bit(i as u8);
            stack.push(c);
        }
    }
    BranchNode::new(stack, mask)
}

// ---------------------------------------------------------------------------
// Update by path copying
// ---------------------------------------------------------------------------

fn prefix_key<E>(key: &Nibbles) -> TrieError<E> {
    TrieError::PrefixKey(key.pack().to_vec())
}

/// Rebuild the subtree `node` at `depth` under `entries`. Every entry key
/// shares its first `depth` nibbles with the subtree's path; entries are
/// sorted and unique. `None` in is an empty subtree, `None` out is an emptied
/// one.
fn update_node<S: NodeReader + NodeSink>(
    store: &mut S,
    node: Option<TrieNode>,
    depth: usize,
    entries: &[Entry],
) -> Result<Option<TrieNode>, TrieError<S::Error>> {
    if entries.is_empty() {
        return Ok(node);
    }
    match node {
        None | Some(TrieNode::EmptyRoot) => build(store, depth, entries),
        Some(TrieNode::Leaf(leaf)) => {
            let mut full = entries[0].0.slice(..depth);
            full.extend(&leaf.key);
            let mut merged = entries.to_vec();
            if let Err(i) = merged.binary_search_by(|(k, _)| k.cmp(&full)) {
                merged.insert(i, (full, Some(leaf.value)));
            }
            build(store, depth, &merged)
        }
        Some(TrieNode::Extension(ext)) => update_extension(store, ext, depth, entries),
        Some(TrieNode::Branch(branch)) => {
            let mut children = children_of(&branch);
            for (nibble, group) in group_by_nibble(depth, entries)? {
                let existing = match &children[nibble as usize] {
                    Some(r) => Some(load(store, r)?),
                    None => None,
                };
                let updated = update_node(store, existing, depth + 1, group)?;
                children[nibble as usize] = updated.map(|n| put(store, &n));
            }
            normalize_branch(store, children)
        }
    }
}

fn update_extension<S: NodeReader + NodeSink>(
    store: &mut S,
    ext: ExtensionNode,
    depth: usize,
    entries: &[Entry],
) -> Result<Option<TrieNode>, TrieError<S::Error>> {
    // A delete of a key outside this extension's subtree is a no-op.
    let relevant: Vec<Entry> = entries
        .iter()
        .filter(|(k, v)| v.is_some() || k.slice(depth..).starts_with(&ext.key))
        .cloned()
        .collect();
    if relevant.is_empty() {
        return Ok(Some(TrieNode::Extension(ext)));
    }
    let shared = relevant
        .iter()
        .map(|(k, _)| k.slice(depth..).common_prefix_length(&ext.key))
        .min()
        .unwrap_or(0);

    if shared == ext.key.len() {
        // Everything lands below the extension: recurse and re-join.
        let child = load(store, &ext.child)?;
        let updated = update_node(store, Some(child), depth + shared, &relevant)?;
        return join(store, &ext.key, updated);
    }

    // The paths diverge inside the extension: a branch at `depth + shared`.
    let fork = depth + shared;
    let own_nibble = ext.key.get_unchecked(shared);
    let own_rest = ext.key.slice(shared + 1..);
    let mut children: [Option<RlpNode>; 16] = Default::default();
    // The extension's own continuation, as a node the recursion can update.
    let own_node = if own_rest.is_empty() {
        load(store, &ext.child)?
    } else {
        TrieNode::Extension(ExtensionNode::new(own_rest, ext.child))
    };
    let mut own_node = Some(own_node);
    for (nibble, group) in group_by_nibble(fork, &relevant)? {
        let updated = if nibble == own_nibble {
            update_node(store, own_node.take(), fork + 1, group)?
        } else {
            build(store, fork + 1, group)?
        };
        children[nibble as usize] = updated.map(|n| put(store, &n));
    }
    if let Some(own) = own_node {
        children[own_nibble as usize] = Some(put(store, &own));
    }
    let branch = normalize_branch(store, children)?;
    join(store, &ext.key.slice(..shared), branch)
}

/// Prepend `prefix` to `node`: an extension over a branch, or a longer key on
/// a leaf or extension.
fn join<S: NodeSink, E>(
    store: &mut S,
    prefix: &Nibbles,
    node: Option<TrieNode>,
) -> Result<Option<TrieNode>, TrieError<E>> {
    if prefix.is_empty() {
        return Ok(node);
    }
    let mut key = *prefix;
    Ok(match node {
        None => None,
        Some(TrieNode::EmptyRoot) => None,
        Some(TrieNode::Leaf(leaf)) => {
            key.extend(&leaf.key);
            Some(TrieNode::Leaf(LeafNode::new(key, leaf.value)))
        }
        Some(TrieNode::Extension(ext)) => {
            key.extend(&ext.key);
            Some(TrieNode::Extension(ExtensionNode::new(key, ext.child)))
        }
        Some(branch @ TrieNode::Branch(_)) => {
            let child = put(store, &branch);
            Some(TrieNode::Extension(ExtensionNode::new(key, child)))
        }
    })
}

/// A branch with zero children vanishes; with one child it collapses into
/// that child, one nibble longer.
fn normalize_branch<S: NodeReader + NodeSink>(
    store: &mut S,
    children: [Option<RlpNode>; 16],
) -> Result<Option<TrieNode>, TrieError<S::Error>> {
    let mut present = children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.as_ref().map(|c| (i as u8, c)));
    let Some((nibble, only)) = present.next() else {
        return Ok(None);
    };
    if present.next().is_some() {
        return Ok(Some(TrieNode::Branch(branch_from(children))));
    }
    let child = load(store, only)?;
    let mut prefix = Nibbles::new();
    prefix.push(nibble);
    join(store, &prefix, Some(child))
}

/// Build a fresh subtree at `depth` from the inserts in `entries`.
fn build<S: NodeSink, E>(
    store: &mut S,
    depth: usize,
    entries: &[Entry],
) -> Result<Option<TrieNode>, TrieError<E>> {
    let inserts: Vec<(&Nibbles, &Vec<u8>)> = entries
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k, v)))
        .collect();
    build_inserts(store, depth, &inserts)
}

fn build_inserts<S: NodeSink, E>(
    store: &mut S,
    depth: usize,
    inserts: &[(&Nibbles, &Vec<u8>)],
) -> Result<Option<TrieNode>, TrieError<E>> {
    match inserts {
        [] => Ok(None),
        [(key, value)] => Ok(Some(TrieNode::Leaf(LeafNode::new(
            key.slice(depth..),
            (*value).clone(),
        )))),
        _ => {
            // Sorted, so the common prefix of all is that of the first and last.
            let first = inserts[0].0.slice(depth..);
            let last = inserts[inserts.len() - 1].0.slice(depth..);
            let shared = first.common_prefix_length(&last);
            let branch = build_branch(store, depth + shared, inserts)?;
            join(store, &first.slice(..shared), Some(branch))
        }
    }
}

/// A branch at `depth` over `inserts`, which are known to differ at `depth`.
fn build_branch<S: NodeSink, E>(
    store: &mut S,
    depth: usize,
    inserts: &[(&Nibbles, &Vec<u8>)],
) -> Result<TrieNode, TrieError<E>> {
    let mut children: [Option<RlpNode>; 16] = Default::default();
    let mut i = 0;
    while i < inserts.len() {
        let nibble = inserts[i]
            .0
            .get(depth)
            .ok_or_else(|| prefix_key(inserts[i].0))?;
        let start = i;
        while i < inserts.len() && inserts[i].0.get(depth) == Some(nibble) {
            i += 1;
        }
        let child = build_inserts(store, depth + 1, &inserts[start..i])?
            .expect("a non-empty insert group builds a node");
        children[nibble as usize] = Some(put(store, &child));
    }
    Ok(TrieNode::Branch(branch_from(children)))
}

/// Split sorted `entries` into runs by their nibble at `depth`.
fn group_by_nibble<E>(
    depth: usize,
    entries: &[Entry],
) -> Result<Vec<(u8, &[Entry])>, TrieError<E>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let nibble = entries[i]
            .0
            .get(depth)
            .ok_or_else(|| prefix_key(&entries[i].0))?;
        let start = i;
        while i < entries.len() && entries[i].0.get(depth) == Some(nibble) {
            i += 1;
        }
        groups.push((nibble, &entries[start..i]));
    }
    Ok(groups)
}

// ---------------------------------------------------------------------------
// Ordered iteration
// ---------------------------------------------------------------------------

enum Frame {
    /// A node already loaded, at this path.
    Node(Nibbles, TrieNode),
    /// A child reference not yet loaded, at this path.
    Ref(Nibbles, RlpNode),
}

/// In-order walk from a lower bound. See [`Trie::iter_from`].
pub struct TrieIter<'a, S> {
    store: &'a S,
    lower: Nibbles,
    stack: Vec<Frame>,
}

impl<S: NodeReader> TrieIter<'_, S> {
    /// Can the subtree at `path` hold a key `>= lower`?
    fn may_reach(&self, path: &Nibbles) -> bool {
        let n = path.len().min(self.lower.len());
        path.slice(..n).cmp(&self.lower.slice(..n)) != Ordering::Less
    }
}

impl<S: NodeReader> Iterator for TrieIter<'_, S> {
    type Item = Item<S::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (path, node) = match self.stack.pop()? {
                Frame::Node(path, node) => (path, node),
                Frame::Ref(path, r) => match load(self.store, &r) {
                    Ok(node) => (path, node),
                    Err(e) => return Some(Err(e)),
                },
            };
            match node {
                TrieNode::EmptyRoot => {}
                TrieNode::Leaf(leaf) => {
                    let mut key = path;
                    key.extend(&leaf.key);
                    if key.cmp(&self.lower) != Ordering::Less {
                        return Some(Ok((key.pack().to_vec(), leaf.value)));
                    }
                }
                TrieNode::Extension(ext) => {
                    let mut child_path = path;
                    child_path.extend(&ext.key);
                    if self.may_reach(&child_path) {
                        self.stack.push(Frame::Ref(child_path, ext.child));
                    }
                }
                TrieNode::Branch(branch) => {
                    // Push in reverse so the smallest nibble pops first.
                    let children = children_of(&branch);
                    for (nibble, child) in children.into_iter().enumerate().rev() {
                        let Some(child) = child else { continue };
                        let mut child_path = path;
                        child_path.push(nibble as u8);
                        if self.may_reach(&child_path) {
                            self.stack.push(Frame::Ref(child_path, child));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemNodeStore;
    use alloy_trie::HashBuilder;

    /// A tiny deterministic generator, so the tests need no `rand`.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.next() as u8).collect()
        }
    }

    type Model = BTreeMap<Vec<u8>, Vec<u8>>;

    /// The reference root: alloy-trie's hash builder over the sorted leaves.
    fn reference_root(map: &Model) -> B256 {
        let mut hb = HashBuilder::default();
        for (k, v) in map {
            hb.add_leaf(Nibbles::unpack(k), v);
        }
        hb.root()
    }

    fn apply(map: &mut Model, changes: &Changes) {
        for (k, v) in changes {
            match v {
                Some(v) => {
                    map.insert(k.clone(), v.clone());
                }
                None => {
                    map.remove(k);
                }
            }
        }
    }

    fn collect_all(store: &MemNodeStore, root: B256) -> Vec<(Vec<u8>, Vec<u8>)> {
        Trie::iter_from(store, root, &[])
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn empty_root_is_the_empty_hash() {
        let mut store = MemNodeStore::new();
        let root = Trie::update(&mut store, EMPTY_ROOT_HASH, &Changes::new()).unwrap();
        assert_eq!(root, EMPTY_ROOT_HASH);
        assert_eq!(Trie::get(&store, root, b"x").unwrap(), None);
        assert!(collect_all(&store, root).is_empty());
    }

    #[test]
    fn single_leaf_matches_reference() {
        let mut store = MemNodeStore::new();
        let mut changes = Changes::new();
        changes.insert(vec![1, 2, 3], Some(b"hello".to_vec()));
        let root = Trie::update(&mut store, EMPTY_ROOT_HASH, &changes).unwrap();
        let mut map = BTreeMap::new();
        apply(&mut map, &changes);
        assert_eq!(root, reference_root(&map));
        assert_eq!(
            Trie::get(&store, root, &[1, 2, 3]).unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(Trie::get(&store, root, &[1, 2]).unwrap(), None);
        assert_eq!(Trie::get(&store, root, &[1, 2, 4]).unwrap(), None);
    }

    #[test]
    fn random_batches_match_reference_and_old_roots_survive() {
        let mut rng = Lcg(7);
        let mut store = MemNodeStore::new();
        let mut map = Model::new();
        let mut root = EMPTY_ROOT_HASH;
        let mut history: Vec<(B256, Model)> = Vec::new();
        let mut keys: Vec<Vec<u8>> = Vec::new();

        for round in 0..40 {
            let mut changes = Changes::new();
            let n = 1 + (rng.next() % 30) as usize;
            for _ in 0..n {
                // Mostly fresh 32-byte keys with a shared prefix now and then,
                // some updates and deletes of known keys.
                let roll = rng.next() % 10;
                if roll < 6 || keys.is_empty() {
                    let mut k = rng.bytes(32);
                    if rng.next().is_multiple_of(3) && !keys.is_empty() {
                        let base = &keys[(rng.next() as usize) % keys.len()];
                        let p = (rng.next() % 32) as usize;
                        k[..p].copy_from_slice(&base[..p]);
                    }
                    keys.push(k.clone());
                    let len = 1 + (rng.next() % 80) as usize;
                    changes.insert(k, Some(rng.bytes(len)));
                } else if roll < 8 {
                    let k = keys[(rng.next() as usize) % keys.len()].clone();
                    changes.insert(k, Some(rng.bytes(40)));
                } else {
                    let k = keys[(rng.next() as usize) % keys.len()].clone();
                    changes.insert(k, None);
                }
            }
            root = Trie::update(&mut store, root, &changes).unwrap();
            apply(&mut map, &changes);
            assert_eq!(root, reference_root(&map), "round {round}");
            let listed = collect_all(&store, root);
            let expected: Vec<_> = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(listed, expected, "round {round} iteration");
            for (k, v) in &map {
                assert_eq!(Trie::get(&store, root, k).unwrap().as_ref(), Some(v));
            }
            history.push((root, map.clone()));
        }

        // Persistence: every earlier root still answers as it did.
        for (old_root, old_map) in &history {
            assert_eq!(*old_root, reference_root(old_map));
            let listed = collect_all(&store, *old_root);
            let expected: Vec<_> = old_map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(listed, expected);
        }
    }

    #[test]
    fn delete_everything_returns_to_empty() {
        let mut rng = Lcg(3);
        let mut store = MemNodeStore::new();
        let mut changes = Changes::new();
        for _ in 0..50 {
            changes.insert(rng.bytes(8), Some(rng.bytes(4)));
        }
        let root = Trie::update(&mut store, EMPTY_ROOT_HASH, &changes).unwrap();
        let deletes: Changes = changes.keys().map(|k| (k.clone(), None)).collect();
        let root = Trie::update(&mut store, root, &deletes).unwrap();
        assert_eq!(root, EMPTY_ROOT_HASH);
    }

    #[test]
    fn deleting_an_absent_key_is_a_no_op() {
        let mut store = MemNodeStore::new();
        let mut changes = Changes::new();
        changes.insert(vec![0xaa, 0x01], Some(vec![1]));
        changes.insert(vec![0xaa, 0x02], Some(vec![2]));
        let root = Trie::update(&mut store, EMPTY_ROOT_HASH, &changes).unwrap();
        let mut deletes = Changes::new();
        deletes.insert(vec![0xbb, 0x01], None);
        deletes.insert(vec![0xaa, 0x03], None);
        deletes.insert(vec![0xaa], None);
        let root2 = Trie::update(&mut store, root, &deletes).unwrap();
        assert_eq!(root, root2);
    }

    #[test]
    fn prefix_keys_are_rejected() {
        let mut store = MemNodeStore::new();
        let mut changes = Changes::new();
        changes.insert(vec![0xaa], Some(vec![1]));
        changes.insert(vec![0xaa, 0x02], Some(vec![2]));
        assert!(matches!(
            Trie::update(&mut store, EMPTY_ROOT_HASH, &changes),
            Err(TrieError::PrefixKey(_))
        ));
    }

    #[test]
    fn range_scans_respect_bounds() {
        let mut store = MemNodeStore::new();
        let mut changes = Changes::new();
        for i in 0u16..200 {
            changes.insert(i.to_be_bytes().to_vec(), Some(vec![i as u8]));
        }
        let root = Trie::update(&mut store, EMPTY_ROOT_HASH, &changes).unwrap();
        let got: Vec<u16> = Trie::range(
            &store,
            root,
            &50u16.to_be_bytes(),
            Some(&60u16.to_be_bytes()),
        )
        .unwrap()
        .map(|r| u16::from_be_bytes(r.unwrap().0.try_into().unwrap()))
        .collect();
        assert_eq!(got, (50..60).collect::<Vec<_>>());
        // A lower bound between keys starts at the next key.
        let got: Vec<u16> = Trie::iter_from(&store, root, &[0, 100, 1])
            .unwrap()
            .take(2)
            .map(|r| u16::from_be_bytes(r.unwrap().0.try_into().unwrap()))
            .collect();
        assert_eq!(got, vec![101, 102]);
    }

    #[test]
    fn staging_reads_its_own_writes_and_hands_them_over() {
        let mut base = MemNodeStore::new();
        let mut changes = Changes::new();
        changes.insert(vec![1; 32], Some(vec![1]));
        let root = Trie::update(&mut base, EMPTY_ROOT_HASH, &changes).unwrap();

        let mut staging = crate::store::Staging::new(&base);
        let mut more = Changes::new();
        more.insert(vec![2; 32], Some(vec![2]));
        let root2 = Trie::update(&mut staging, root, &more).unwrap();
        assert_eq!(Trie::get(&staging, root2, &[2; 32]).unwrap(), Some(vec![2]));
        assert_eq!(Trie::get(&staging, root2, &[1; 32]).unwrap(), Some(vec![1]));
        // The base does not see the new root until the staged nodes are flushed.
        assert!(matches!(
            Trie::get(&base, root2, &[2; 32]),
            Err(TrieError::MissingNode(_))
        ));
        for (h, rlp) in staging.into_staged().drain() {
            base.put_node(h, rlp);
        }
        assert_eq!(Trie::get(&base, root2, &[2; 32]).unwrap(), Some(vec![2]));
    }
}
