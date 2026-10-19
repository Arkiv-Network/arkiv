//! An in-process mirror of the committed state at one commit, normally head.
//!
//! # Why it exists
//!
//! Valkey holds one hash per record and nothing that orders or indexes them,
//! so every query, every count and every index root used to be answered by
//! reading the **whole** state back over the wire, one `HGETALL` per record.
//! That is invisible on a devnet that starts empty and fatal on one that
//! starts with a prefilled genesis: at a few hundred thousand entities a block
//! spent minutes in `seal` before it could be built.
//!
//! The mirror makes those reads memory scans. Valkey stays the only durable
//! record and the mirror is rebuilt from it — on first use, or when another
//! writer has moved the head under it — so losing it costs a reload, never
//! data.
//!
//! # What it holds
//!
//! Every committed record, in [`RecordKey`] order (the order a query without a
//! sort returns), with its **attribute** cells split out as a ready-made
//! [`Record`]. A filter only ever reads attributes, so a scan matches against
//! that and never copies a payload; the full record is built for the page that
//! is returned and nothing else.
//!
//! It also carries the index root at its commit, kept incrementally, which is
//! what turns [`index_hash`](crate::backend::Backend::index_hash) from a scan
//! of the state into a fold over a branch's diff.

use std::collections::BTreeMap;
use std::collections::btree_map;
use std::iter::Peekable;

use arkiv_interfaces::store::{CellKind, Record, RecordKey};

use crate::codec::{self, Stored};

/// One committed record, with the part a filter reads split out.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) stored: Stored,
    /// The attribute cells alone, as a [`Record`]: what `Filter::matches`
    /// reads. Possibly empty — an account record has no attributes, and an
    /// empty filter must still see it.
    pub(crate) attributes: Record,
}

impl Entry {
    pub(crate) fn new(key: RecordKey, stored: Stored) -> Self {
        let attributes = Record {
            key,
            version: stored.version,
            cells: stored
                .cells
                .iter()
                .filter(|(_, cell)| cell.kind == CellKind::Attribute)
                .map(|(name, cell)| (name.clone(), cell.clone()))
                .collect(),
        };
        Self { stored, attributes }
    }
}

/// A record's contribution to the index root, or `None` when it has no
/// attribute cell and so contributes nothing.
///
/// The same value the index root has always been folded from: the record
/// digest over its attribute cells alone.
pub(crate) fn index_digest(key: RecordKey, record: &Stored) -> Option<[u8; 32]> {
    let attributes: BTreeMap<_, _> = record
        .cells
        .iter()
        .filter(|(_, cell)| cell.kind == CellKind::Attribute)
        .map(|(name, cell)| (name.clone(), cell.clone()))
        .collect();
    if attributes.is_empty() {
        return None;
    }
    Some(codec::record_digest(
        key,
        &Stored {
            version: record.version,
            cells: attributes,
        },
    ))
}

/// Replace `before` with `after` in an index root.
pub(crate) fn fold_index(
    root: &mut [u8; 32],
    key: RecordKey,
    before: Option<&Stored>,
    after: Option<&Stored>,
) {
    if let Some(digest) = before.and_then(|record| index_digest(key, record)) {
        codec::fold_digest(root, digest);
    }
    if let Some(digest) = after.and_then(|record| index_digest(key, record)) {
        codec::fold_digest(root, digest);
    }
}

/// The committed state as of [`head`](Self::head).
#[derive(Debug, Default)]
pub(crate) struct HeadCache {
    /// The commit this mirrors.
    pub(crate) head: u64,
    pub(crate) records: BTreeMap<RecordKey, Entry>,
    /// The index root at `head`.
    pub(crate) index_root: [u8; 32],
}

impl HeadCache {
    /// A mirror of `records` as of `head`, with its index root computed.
    pub(crate) fn build(head: u64, records: BTreeMap<RecordKey, Stored>) -> Self {
        let mut index_root = [0u8; 32];
        let records = records
            .into_iter()
            .map(|(key, stored)| {
                if let Some(digest) = index_digest(key, &stored) {
                    codec::fold_digest(&mut index_root, digest);
                }
                (key, Entry::new(key, stored))
            })
            .collect();
        Self {
            head,
            records,
            index_root,
        }
    }

    /// Apply one record's change, keeping the index root in step.
    pub(crate) fn apply(&mut self, key: RecordKey, after: Option<Stored>) {
        let before = self.records.remove(&key);
        fold_index(
            &mut self.index_root,
            key,
            before.as_ref().map(|entry| &entry.stored),
            after.as_ref(),
        );
        if let Some(stored) = after {
            self.records.insert(key, Entry::new(key, stored));
        }
    }
}

/// The records as of some commit at or before the mirror's: the mirror, with
/// `overlay` substituted wherever a later commit changed a record. `None` in
/// the overlay is a record that did not exist yet.
///
/// Both sides are in key order, so this is a merge, and the result is in key
/// order too.
pub(crate) struct View<'a> {
    base: Peekable<btree_map::Iter<'a, RecordKey, Entry>>,
    overlay: Peekable<btree_map::Iter<'a, RecordKey, Option<Entry>>>,
}

impl<'a> View<'a> {
    pub(crate) fn new(
        base: &'a BTreeMap<RecordKey, Entry>,
        overlay: &'a BTreeMap<RecordKey, Option<Entry>>,
    ) -> Self {
        Self {
            base: base.iter().peekable(),
            overlay: overlay.iter().peekable(),
        }
    }
}

impl<'a> Iterator for View<'a> {
    type Item = (RecordKey, &'a Entry);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next_base = self.base.peek().map(|(key, _)| **key);
            let next_overlay = self.overlay.peek().map(|(key, _)| **key);
            let take_overlay = match (next_base, next_overlay) {
                (None, None) => return None,
                (_, None) => false,
                (None, Some(_)) => true,
                (Some(base), Some(overlay)) => overlay <= base,
            };
            if !take_overlay {
                let (key, entry) = self.base.next()?;
                return Some((*key, entry));
            }
            // The overlay wins for its key; drop the base's copy of it.
            if next_base == next_overlay {
                self.base.next();
            }
            let (key, entry) = self.overlay.next()?;
            if let Some(entry) = entry {
                return Some((*key, entry));
            }
            // Absent at that commit: skip it and look again.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::store::{Cell, RecordVersion, TypeId};

    fn stored(attribute: Option<u8>, field: u8) -> Stored {
        let mut cells = BTreeMap::new();
        if let Some(value) = attribute {
            cells.insert("a".to_owned(), Cell::attribute(TypeId::U64, vec![value; 8]));
        }
        cells.insert("f".to_owned(), Cell::field(TypeId::BYTES, vec![field]));
        Stored {
            version: RecordVersion(1),
            cells,
        }
    }

    fn key(byte: u8) -> RecordKey {
        RecordKey([byte; 32])
    }

    #[test]
    fn the_incremental_index_root_matches_a_rebuild() {
        let mut cache = HeadCache::build(
            1,
            [(key(1), stored(Some(1), 1)), (key(2), stored(None, 2))].into(),
        );
        cache.apply(key(1), Some(stored(Some(9), 1)));
        cache.apply(key(3), Some(stored(Some(3), 3)));
        cache.apply(key(2), None);
        let rebuilt = HeadCache::build(
            2,
            cache
                .records
                .iter()
                .map(|(key, entry)| (*key, entry.stored.clone()))
                .collect(),
        );
        assert_eq!(cache.index_root, rebuilt.index_root);
    }

    #[test]
    fn a_field_only_change_leaves_the_index_root_alone() {
        let mut cache = HeadCache::build(1, [(key(1), stored(Some(1), 1))].into());
        let before = cache.index_root;
        cache.apply(key(1), Some(stored(Some(1), 2)));
        assert_eq!(cache.index_root, before);
    }

    #[test]
    fn a_view_substitutes_removes_and_restores_in_key_order() {
        let base: BTreeMap<_, _> = [1u8, 3, 5]
            .into_iter()
            .map(|b| (key(b), Entry::new(key(b), stored(Some(b), b))))
            .collect();
        let overlay: BTreeMap<_, _> = [
            (key(0), Some(Entry::new(key(0), stored(Some(0), 0)))),
            (key(3), None),
            (key(4), Some(Entry::new(key(4), stored(Some(4), 4)))),
            (key(5), Some(Entry::new(key(5), stored(Some(50), 5)))),
        ]
        .into();
        let seen: Vec<_> = View::new(&base, &overlay)
            .map(|(key, entry)| (key.0[0], entry.stored.cells["a"].value[0]))
            .collect();
        assert_eq!(seen, [(0, 0), (1, 1), (4, 4), (5, 50)]);
    }
}
