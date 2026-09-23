//! The trie operations: lookup, batch update by path copying, ordered
//! iteration. All of them are free functions on [`Trie`] over a root hash and
//! a node store; there is no trie object, because a root *is* the trie.

use alloy_primitives::{B256, keccak256};
use alloy_trie::EMPTY_ROOT_HASH;
use core::cmp::Ordering;
use std::collections::BTreeMap;

use crate::node::{Child, DecodeError, Node, Path};
use crate::store::{NodeReader, NodeSink};

#[derive(Debug)]
pub enum TrieError<E> {
    Store(E),
    /// A node the trie refers to is not in the store.
    MissingNode(B256),
    Decode(DecodeError),
    /// The key is a prefix of another key in the trie.
    PrefixKey(Vec<u8>),
}

/// A batch of changes: `Some` inserts or replaces, `None` deletes. A `BTreeMap`
/// so the keys are unique and sorted.
pub type Changes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// One `(key, value)` entry of a walk.
pub type Item<E> = Result<(Vec<u8>, Vec<u8>), TrieError<E>>;

type Entry = (Path, Option<Vec<u8>>);

/// The operations. See the module docs.
pub struct Trie;

impl Trie {
    /// The value under `key` in the trie at `root`.
    pub fn get<S: NodeReader>(
        store: &S,
        root: B256,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TrieError<S::Error>> {
        let key = Path::unpack(key);
        let mut depth = 0;
        let mut node = match load_root(store, root)? {
            Some(n) => n,
            None => return Ok(None),
        };
        loop {
            match node {
                Node::Leaf { path, value } => {
                    return Ok((path.as_slice() == &key.as_slice()[depth..]).then_some(value));
                }
                Node::Extension { path, child } => {
                    if !key.as_slice()[depth..].starts_with(path.as_slice()) {
                        return Ok(None);
                    }
                    depth += path.len();
                    node = load(store, &child)?;
                }
                Node::Branch { children } => {
                    let Some(nibble) = key.get(depth) else {
                        return Ok(None);
                    };
                    let Some(child) = &children[nibble as usize] else {
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
            .map(|(k, v)| (Path::unpack(k), v.clone()))
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
            stack.push(Frame::Node(Path::new(), node));
        }
        Ok(TrieIter {
            store,
            lower: Path::unpack(from),
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

fn decode<E>(rlp: &[u8]) -> Result<Node, TrieError<E>> {
    Node::decode(rlp).map_err(TrieError::Decode)
}

fn load_root<S: NodeReader>(store: &S, root: B256) -> Result<Option<Node>, TrieError<S::Error>> {
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
fn load<S: NodeReader>(store: &S, child: &Child) -> Result<Node, TrieError<S::Error>> {
    match child {
        Child::Hash(hash) => {
            let rlp = store
                .node(hash)
                .map_err(TrieError::Store)?
                .ok_or(TrieError::MissingNode(*hash))?;
            decode(&rlp)
        }
        Child::Inline(rlp) => decode(rlp),
    }
}

/// Encode `node` and store it if it is hash-referenced; return the reference
/// a parent embeds.
fn put<S: NodeSink>(store: &mut S, node: &Node) -> Child {
    let rlp = node.encode();
    let child = Child::of(&rlp);
    if let Child::Hash(hash) = &child {
        store.put_node(*hash, rlp);
    }
    child
}

/// The root is always referenced by hash, even when its encoding is short.
fn put_root<S: NodeSink>(store: &mut S, node: &Node) -> B256 {
    let rlp = node.encode();
    let hash = keccak256(&rlp);
    store.put_node(hash, rlp);
    hash
}

// ---------------------------------------------------------------------------
// Update by path copying
// ---------------------------------------------------------------------------

fn prefix_key<E>(key: &Path) -> TrieError<E> {
    TrieError::PrefixKey(key.pack())
}

/// Rebuild the subtree `node` at `depth` under `entries`. Every entry key
/// shares its first `depth` nibbles with the subtree's path; entries are
/// sorted and unique. `None` in is an empty subtree, `None` out is an emptied
/// one.
fn update_node<S: NodeReader + NodeSink>(
    store: &mut S,
    node: Option<Node>,
    depth: usize,
    entries: &[Entry],
) -> Result<Option<Node>, TrieError<S::Error>> {
    if entries.is_empty() {
        return Ok(node);
    }
    match node {
        None => build(store, depth, entries),
        Some(Node::Leaf { path, value }) => {
            let mut full = entries[0].0.slice(0..depth);
            full.extend(&path);
            let mut merged = entries.to_vec();
            if let Err(i) = merged.binary_search_by(|(k, _)| k.cmp(&full)) {
                merged.insert(i, (full, Some(value)));
            }
            build(store, depth, &merged)
        }
        Some(Node::Extension { path, child }) => {
            update_extension(store, path, child, depth, entries)
        }
        Some(Node::Branch { children }) => {
            let mut children = *children;
            for (nibble, group) in group_by_nibble(depth, entries)? {
                let existing = match &children[nibble as usize] {
                    Some(c) => Some(load(store, c)?),
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
    ext_path: Path,
    ext_child: Child,
    depth: usize,
    entries: &[Entry],
) -> Result<Option<Node>, TrieError<S::Error>> {
    // A delete of a key outside this extension's subtree is a no-op.
    let relevant: Vec<Entry> = entries
        .iter()
        .filter(|(k, v)| v.is_some() || k.slice_from(depth).starts_with(&ext_path))
        .cloned()
        .collect();
    if relevant.is_empty() {
        return Ok(Some(Node::Extension {
            path: ext_path,
            child: ext_child,
        }));
    }
    let shared = relevant
        .iter()
        .map(|(k, _)| k.slice_from(depth).common_prefix_length(&ext_path))
        .min()
        .unwrap_or(0);

    if shared == ext_path.len() {
        // Everything lands below the extension: recurse and re-join.
        let child = load(store, &ext_child)?;
        let updated = update_node(store, Some(child), depth + shared, &relevant)?;
        return join(store, &ext_path, updated);
    }

    // The paths diverge inside the extension: a branch at `depth + shared`.
    let fork = depth + shared;
    let own_nibble = ext_path.get(shared).expect("shared < len");
    let own_rest = ext_path.slice_from(shared + 1);
    let mut children: [Option<Child>; 16] = Default::default();
    // The extension's own continuation, as a node the recursion can update.
    let mut own_node = Some(if own_rest.is_empty() {
        load(store, &ext_child)?
    } else {
        Node::Extension {
            path: own_rest,
            child: ext_child,
        }
    });
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
    join(store, &ext_path.slice(0..shared), branch)
}

/// Prepend `prefix` to `node`: an extension over a branch, or a longer key on
/// a leaf or extension.
fn join<S: NodeSink, E>(
    store: &mut S,
    prefix: &Path,
    node: Option<Node>,
) -> Result<Option<Node>, TrieError<E>> {
    if prefix.is_empty() {
        return Ok(node);
    }
    let mut key = prefix.clone();
    Ok(match node {
        None => None,
        Some(Node::Leaf { path, value }) => {
            key.extend(&path);
            Some(Node::Leaf { path: key, value })
        }
        Some(Node::Extension { path, child }) => {
            key.extend(&path);
            Some(Node::Extension { path: key, child })
        }
        Some(branch @ Node::Branch { .. }) => {
            let child = put(store, &branch);
            Some(Node::Extension { path: key, child })
        }
    })
}

/// A branch with zero children vanishes; with one child it collapses into
/// that child, one nibble longer.
fn normalize_branch<S: NodeReader + NodeSink>(
    store: &mut S,
    children: [Option<Child>; 16],
) -> Result<Option<Node>, TrieError<S::Error>> {
    let mut present = children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.as_ref().map(|c| (i as u8, c)));
    let Some((nibble, only)) = present.next() else {
        return Ok(None);
    };
    if present.next().is_some() {
        return Ok(Some(Node::branch(children)));
    }
    let child = load(store, only)?;
    let mut prefix = Path::new();
    prefix.push(nibble);
    join(store, &prefix, Some(child))
}

/// Build a fresh subtree at `depth` from the inserts in `entries`.
fn build<S: NodeSink, E>(
    store: &mut S,
    depth: usize,
    entries: &[Entry],
) -> Result<Option<Node>, TrieError<E>> {
    let inserts: Vec<(&Path, &Vec<u8>)> = entries
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k, v)))
        .collect();
    build_inserts(store, depth, &inserts)
}

fn build_inserts<S: NodeSink, E>(
    store: &mut S,
    depth: usize,
    inserts: &[(&Path, &Vec<u8>)],
) -> Result<Option<Node>, TrieError<E>> {
    match inserts {
        [] => Ok(None),
        [(key, value)] => Ok(Some(Node::Leaf {
            path: key.slice_from(depth),
            value: (*value).clone(),
        })),
        _ => {
            // Sorted, so the common prefix of all is that of the first and last.
            let first = inserts[0].0.slice_from(depth);
            let last = inserts[inserts.len() - 1].0.slice_from(depth);
            let shared = first.common_prefix_length(&last);
            let branch = build_branch(store, depth + shared, inserts)?;
            join(store, &first.slice(0..shared), Some(branch))
        }
    }
}

/// A branch at `depth` over `inserts`, which are known to differ at `depth`.
fn build_branch<S: NodeSink, E>(
    store: &mut S,
    depth: usize,
    inserts: &[(&Path, &Vec<u8>)],
) -> Result<Node, TrieError<E>> {
    let mut children: [Option<Child>; 16] = Default::default();
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
    Ok(Node::branch(children))
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
    Node(Path, Node),
    /// A child reference not yet loaded, at this path.
    Ref(Path, Child),
}

/// In-order walk from a lower bound. See [`Trie::iter_from`].
pub struct TrieIter<'a, S> {
    store: &'a S,
    lower: Path,
    stack: Vec<Frame>,
}

impl<S: NodeReader> TrieIter<'_, S> {
    /// Can the subtree at `path` hold a key `>= lower`?
    fn may_reach(&self, path: &Path) -> bool {
        let n = path.len().min(self.lower.len());
        path.cmp_prefix(&self.lower, n) != Ordering::Less
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
                Node::Leaf { path: rest, value } => {
                    let mut key = path;
                    key.extend(&rest);
                    if key.cmp(&self.lower) != Ordering::Less {
                        return Some(Ok((key.pack(), value)));
                    }
                }
                Node::Extension { path: rest, child } => {
                    let mut child_path = path;
                    child_path.extend(&rest);
                    if self.may_reach(&child_path) {
                        self.stack.push(Frame::Ref(child_path, child));
                    }
                }
                Node::Branch { children } => {
                    // Push in reverse so the smallest nibble pops first.
                    for (nibble, child) in children.into_iter().enumerate().rev() {
                        let Some(child) = child else { continue };
                        let mut child_path = path.clone();
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
    use alloy_trie::{HashBuilder, Nibbles};

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

    /// Keys longer than 32 bytes, which alloy-trie cannot represent, work
    /// here: an index key is a value followed by a 32-byte entity key.
    #[test]
    fn long_keys_are_supported() {
        let mut rng = Lcg(11);
        let mut store = MemNodeStore::new();
        let mut changes = Changes::new();
        let mut model = Model::new();
        for i in 0..300u32 {
            let mut k = format!("team-{}", i % 7).into_bytes();
            k.extend_from_slice(&[0, 0]);
            k.extend_from_slice(&rng.bytes(32));
            changes.insert(k.clone(), Some(vec![1]));
            model.insert(k, vec![1]);
        }
        let root = Trie::update(&mut store, EMPTY_ROOT_HASH, &changes).unwrap();
        let listed = collect_all(&store, root);
        let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(listed, expected);
        let team3: Vec<_> = Trie::range(&store, root, b"team-3\0\0", Some(b"team-3\0\x01"))
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(
            team3.len(),
            model.keys().filter(|k| k.starts_with(b"team-3")).count()
        );
        for (k, v) in &model {
            assert_eq!(Trie::get(&store, root, k).unwrap().as_ref(), Some(v));
        }
        let deletes: Changes = model.keys().map(|k| (k.clone(), None)).collect();
        assert_eq!(
            Trie::update(&mut store, root, &deletes).unwrap(),
            EMPTY_ROOT_HASH
        );
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
