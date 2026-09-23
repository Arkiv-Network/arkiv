//! A persistent Merkle-Patricia trie over a content-addressed node store.
//!
//! Every node is stored under `keccak(rlp(node))`, so a root hash names an
//! immutable trie and an update is path copying: the nodes on the way to each
//! changed key are rebuilt, everything else is shared with the old root. Old
//! roots stay readable as long as their nodes are kept. Node encoding, and so
//! every root, is Ethereum's (`alloy-trie`), so a proof verifies with ordinary
//! tooling.
//!
//! Keys in one trie must be prefix-free: a key that ends at a branch node is
//! rejected with [`TrieError::PrefixKey`], because the standard branch node
//! carries no value. Keys are byte strings; iteration is in byte order.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod node;
pub mod store;
mod trie;

pub use alloy_trie::EMPTY_ROOT_HASH;
pub use node::{Child, DecodeError, Node, Path};
pub use store::{Layered, LayeredError, MemNodeStore, Never, NodeReader, NodeSink, Staging};
pub use trie::{Changes, Item, Trie, TrieError, TrieIter};
