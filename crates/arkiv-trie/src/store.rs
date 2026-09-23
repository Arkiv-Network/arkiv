//! The node store: `keccak(rlp(node)) -> rlp(node)`.
//!
//! Nodes are immutable and content-addressed, so a store only ever grows and a
//! `put` of an existing hash is a no-op. Reads are `&self` so one store can
//! serve many readers; writes go through a separate seam so a backend can
//! batch them into one transaction.

use alloy_primitives::B256;
use core::fmt::Debug;
use std::collections::HashMap;

/// Read side of a node store.
pub trait NodeReader {
    type Error: Debug;

    /// The RLP bytes of the node stored under `hash`, if any.
    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error>;
}

/// Write side of a node store. Infallible: the trie always writes into an
/// in-memory staging set, and a durable backend flushes that set in one
/// transaction of its own.
pub trait NodeSink {
    /// Store `rlp` under `hash`. The caller guarantees `hash == keccak(rlp)`.
    fn put_node(&mut self, hash: B256, rlp: Vec<u8>);
}

/// An in-memory node store, for tests and for staging a batch of new nodes
/// before they are flushed to a durable backend.
#[derive(Debug, Default, Clone)]
pub struct MemNodeStore {
    nodes: HashMap<B256, Vec<u8>>,
}

impl MemNodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Move every node out, leaving the store empty.
    pub fn drain(&mut self) -> impl Iterator<Item = (B256, Vec<u8>)> + '_ {
        self.nodes.drain()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&B256, &Vec<u8>)> {
        self.nodes.iter()
    }
}

/// The error of a store that cannot fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Never {}

impl NodeReader for MemNodeStore {
    type Error = Never;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, Never> {
        Ok(self.nodes.get(hash).cloned())
    }
}

impl NodeSink for MemNodeStore {
    fn put_node(&mut self, hash: B256, rlp: Vec<u8>) {
        self.nodes.entry(hash).or_insert(rlp);
    }
}

impl<T: NodeReader> NodeReader for std::sync::Arc<T> {
    type Error = T::Error;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, T::Error> {
        (**self).node(hash)
    }
}

impl<T: NodeReader> NodeReader for &T {
    type Error = T::Error;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, T::Error> {
        (**self).node(hash)
    }
}

/// A reader that consults `overlay` first, then `base`. This is how a batch of
/// nodes written during a block is visible to the same block's later reads
/// before the batch reaches the durable store.
#[derive(Debug)]
pub struct Layered<'a, O, B> {
    pub overlay: &'a O,
    pub base: &'a B,
}

impl<'a, O, B> Layered<'a, O, B> {
    pub fn new(overlay: &'a O, base: &'a B) -> Self {
        Self { overlay, base }
    }
}

impl<O, B> NodeReader for Layered<'_, O, B>
where
    O: NodeReader,
    B: NodeReader,
{
    type Error = LayeredError<O::Error, B::Error>;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error> {
        if let Some(n) = self.overlay.node(hash).map_err(LayeredError::Overlay)? {
            return Ok(Some(n));
        }
        self.base.node(hash).map_err(LayeredError::Base)
    }
}

#[derive(Debug)]
pub enum LayeredError<O, B> {
    Overlay(O),
    Base(B),
}

/// A read-write view for one batch of updates: reads see the staged nodes
/// first, then `base`; writes land in the staged set. When the batch is done,
/// [`into_staged`](Staging::into_staged) hands the new nodes to the backend.
#[derive(Debug)]
pub struct Staging<'a, B> {
    staged: MemNodeStore,
    base: &'a B,
}

impl<'a, B> Staging<'a, B> {
    pub fn new(base: &'a B) -> Self {
        Self {
            staged: MemNodeStore::new(),
            base,
        }
    }

    pub fn with_staged(staged: MemNodeStore, base: &'a B) -> Self {
        Self { staged, base }
    }

    pub fn staged(&self) -> &MemNodeStore {
        &self.staged
    }

    pub fn into_staged(self) -> MemNodeStore {
        self.staged
    }
}

impl<B: NodeReader> NodeReader for Staging<'_, B> {
    type Error = B::Error;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, B::Error> {
        if let Some(n) = self.staged.nodes.get(hash) {
            return Ok(Some(n.clone()));
        }
        self.base.node(hash)
    }
}

impl<B> NodeSink for Staging<'_, B> {
    fn put_node(&mut self, hash: B256, rlp: Vec<u8>) {
        self.staged.put_node(hash, rlp);
    }
}
