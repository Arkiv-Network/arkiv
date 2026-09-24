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

/// A durable node store: reads, plus a flush that makes a staged batch
/// durable in one step.
pub trait NodeStore: NodeReader {
    fn flush(&self, staged: MemNodeStore) -> Result<(), Self::Error>;
}

impl<T: NodeStore> NodeStore for std::sync::Arc<T> {
    fn flush(&self, staged: MemNodeStore) -> Result<(), Self::Error> {
        (**self).flush(staged)
    }
}

impl<T: NodeStore> NodeStore for &T {
    fn flush(&self, staged: MemNodeStore) -> Result<(), Self::Error> {
        (**self).flush(staged)
    }
}

/// An in-memory [`NodeStore`] shared between readers, for tests.
#[derive(Debug, Default)]
pub struct SharedMemNodeStore {
    nodes: std::sync::Mutex<MemNodeStore>,
}

impl SharedMemNodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.nodes.lock().expect("poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl NodeReader for SharedMemNodeStore {
    type Error = Never;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, Never> {
        self.nodes.lock().expect("poisoned").node(hash)
    }
}

impl NodeStore for SharedMemNodeStore {
    fn flush(&self, mut staged: MemNodeStore) -> Result<(), Never> {
        let mut nodes = self.nodes.lock().expect("poisoned");
        for (h, rlp) in staged.drain() {
            nodes.put_node(h, rlp);
        }
        Ok(())
    }
}

/// A reader that counts every node lookup it forwards, cache hits included.
/// What a query cost in key-value reads is this number.
#[derive(Debug)]
pub struct CountingReader<S> {
    inner: S,
    reads: core::cell::Cell<u64>,
}

impl<S> CountingReader<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            reads: core::cell::Cell::new(0),
        }
    }

    /// Node lookups so far.
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }
}

impl<S: NodeReader> NodeReader for CountingReader<S> {
    type Error = S::Error;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, S::Error> {
        self.reads.set(self.reads.get() + 1);
        self.inner.node(hash)
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
pub struct Staging<B> {
    staged: MemNodeStore,
    base: B,
}

impl<B> Staging<B> {
    /// Stage over `base`: a `&Store`, an `Arc<Store>`, or any other reader.
    pub fn new(base: B) -> Self {
        Self {
            staged: MemNodeStore::new(),
            base,
        }
    }

    pub fn with_staged(staged: MemNodeStore, base: B) -> Self {
        Self { staged, base }
    }

    pub fn staged(&self) -> &MemNodeStore {
        &self.staged
    }

    pub fn base(&self) -> &B {
        &self.base
    }

    pub fn into_staged(self) -> MemNodeStore {
        self.staged
    }

    pub fn into_parts(self) -> (MemNodeStore, B) {
        (self.staged, self.base)
    }
}

impl<B: NodeReader> NodeReader for Staging<B> {
    type Error = B::Error;

    fn node(&self, hash: &B256) -> Result<Option<Vec<u8>>, B::Error> {
        if let Some(n) = self.staged.nodes.get(hash) {
            return Ok(Some(n.clone()));
        }
        self.base.node(hash)
    }
}

impl<B> NodeSink for Staging<B> {
    fn put_node(&mut self, hash: B256, rlp: Vec<u8>) {
        self.staged.put_node(hash, rlp);
    }
}
