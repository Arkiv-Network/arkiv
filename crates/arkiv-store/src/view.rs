//! [`DbView`]: reads at one database root, and the batch commit that makes the
//! next one.

use core::fmt::Debug;
use core::ops::Bound;
use std::collections::BTreeMap;

use alloy_primitives::B256;
use arkiv_interfaces::entity::{AttributeType, AttributeValue, Entity};
use arkiv_interfaces::primitives::{EntityAddress, EntityCreationNonce, UserAddress};
use arkiv_trie::{Changes, EMPTY_ROOT_HASH, NodeReader, NodeSink, Trie, TrieError};

use crate::annotations::{AttrEntry, annotation_delta};
use crate::encoding::{
    INDEX_PRESENT, entity_of_index_key, index_entry_key, index_id, nonce_from_value, nonce_key,
    nonce_value, str_prefix_key, value_key,
};
use crate::record::{self, RecordError};
use crate::roots::DbRoots;

#[derive(Debug)]
pub enum StoreError<E> {
    Trie(TrieError<E>),
    Store(E),
    Record(RecordError),
    /// The database root names a top node the store does not hold.
    UnknownRoot(B256),
    /// A stored value has the wrong shape.
    Malformed(&'static str),
}

impl<E> From<TrieError<E>> for StoreError<E> {
    fn from(e: TrieError<E>) -> Self {
        Self::Trie(e)
    }
}

/// What one commit changes. Index changes are derived from the entity changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbChanges {
    /// `None` deletes the entity.
    pub entities: BTreeMap<EntityAddress, Option<Entity>>,
    pub nonces: BTreeMap<UserAddress, EntityCreationNonce>,
}

impl DbChanges {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.nonces.is_empty()
    }
}

/// A fallible iterator over `T`.
pub type Stream<'a, T, E> =
    Result<Box<dyn Iterator<Item = Result<T, StoreError<E>>> + 'a>, StoreError<E>>;

/// The database as of one root.
#[derive(Debug)]
pub struct DbView<'a, S> {
    store: &'a S,
    roots: DbRoots,
}

impl<'a, S: NodeReader> DbView<'a, S> {
    /// Open the database at `root`. Errors if the top node is missing.
    pub fn open(store: &'a S, root: B256) -> Result<Self, StoreError<S::Error>> {
        let roots = DbRoots::load(store, root)
            .map_err(StoreError::Store)?
            .ok_or(StoreError::UnknownRoot(root))?;
        Ok(Self { store, roots })
    }

    pub const fn at(store: &'a S, roots: DbRoots) -> Self {
        Self { store, roots }
    }

    pub const fn roots(&self) -> &DbRoots {
        &self.roots
    }

    pub fn root(&self) -> B256 {
        self.roots.root()
    }

    pub const fn store(&self) -> &'a S {
        self.store
    }

    // ── Entities ──────────────────────────────────────────────────────────

    pub fn entity(&self, key: &EntityAddress) -> Result<Option<Entity>, StoreError<S::Error>> {
        match Trie::get(self.store, self.roots.entities, key)? {
            Some(bytes) => record::decode(&bytes).map(Some).map_err(StoreError::Record),
            None => Ok(None),
        }
    }

    /// Every entity key, ascending, starting at `from`.
    pub fn entity_keys_from(&self, from: &EntityAddress) -> Stream<'a, EntityAddress, S::Error>
    where
        S: 'a,
    {
        let iter = Trie::iter_from(self.store, self.roots.entities, from)?;
        Ok(Box::new(iter.map(|item| {
            let (k, _) = item?;
            k.as_slice()
                .try_into()
                .map_err(|_| StoreError::Malformed("entity key length"))
        })))
    }

    pub fn entity_keys(&self) -> Stream<'a, EntityAddress, S::Error>
    where
        S: 'a,
    {
        self.entity_keys_from(&[0; 32])
    }

    /// Every entity, ascending by key.
    pub fn entities(&self) -> Stream<'a, Entity, S::Error>
    where
        S: 'a,
    {
        let iter = Trie::iter_from(self.store, self.roots.entities, &[])?;
        Ok(Box::new(iter.map(|item| {
            let (_, v) = item?;
            record::decode(&v).map_err(StoreError::Record)
        })))
    }

    // ── Nonces ────────────────────────────────────────────────────────────

    pub fn creation_nonce(
        &self,
        owner: &UserAddress,
    ) -> Result<EntityCreationNonce, StoreError<S::Error>> {
        match Trie::get(self.store, self.roots.nonces, &nonce_key(owner))? {
            Some(bytes) => nonce_from_value(&bytes).ok_or(StoreError::Malformed("nonce value")),
            None => Ok(EntityCreationNonce::new(0)),
        }
    }

    // ── Indexes ───────────────────────────────────────────────────────────

    /// The root of the index for `(attr, ty)`, or the empty root.
    pub fn index_root(&self, attr: &[u8], ty: AttributeType) -> Result<B256, StoreError<S::Error>> {
        match Trie::get(self.store, self.roots.indexes, &index_id(attr, ty))? {
            Some(bytes) => Ok(B256::try_from(bytes.as_slice())
                .map_err(|_| StoreError::Malformed("index root length"))?),
            None => Ok(EMPTY_ROOT_HASH),
        }
    }

    /// The entities in the index of `(attr, ty)` whose key lies in
    /// `[from, to)`, ascending by index key. Each index key ends with the
    /// entity key, so within one value the entities come out ascending.
    fn scan(
        &self,
        attr: &[u8],
        ty: AttributeType,
        from: &[u8],
        to: Option<&[u8]>,
    ) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
        let root = self.index_root(attr, ty)?;
        if root == EMPTY_ROOT_HASH {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for item in Trie::range(self.store, root, from, to)? {
            let (k, _) = item?;
            out.push(entity_of_index_key(&k).ok_or(StoreError::Malformed("index key length"))?);
        }
        Ok(out)
    }

    /// Entities with `attr == value`, ascending.
    pub fn equal(
        &self,
        attr: &[u8],
        value: &AttributeValue,
    ) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
        let prefix = value_key(value);
        let mut end = prefix.clone();
        end.extend_from_slice(&[0xff; 32]);
        let mut keys = self.scan(attr, value.attr_type(), &prefix, Some(&end))?;
        // `end` itself is a valid key: include it.
        if Trie::get(self.store, self.index_root(attr, value.attr_type())?, &end)?.is_some() {
            keys.push([0xff; 32]);
        }
        Ok(keys)
    }

    /// String entities whose value starts with `prefix`. The result is sorted
    /// by value then key; the caller sorts by key if it needs that order.
    pub fn prefixed(
        &self,
        attr: &[u8],
        prefix: &str,
    ) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
        let from = str_prefix_key(prefix);
        let to = next_prefix(&from);
        self.scan(attr, AttributeType::Str, &from, to.as_deref())
    }

    /// Entities of type `ty` whose value lies between the bounds. The result
    /// is sorted by value then key.
    pub fn range(
        &self,
        attr: &[u8],
        ty: AttributeType,
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
    ) -> Result<Vec<EntityAddress>, StoreError<S::Error>> {
        // Keys are `value ++ entity`; bounds are on the value prefix alone.
        let from: Vec<u8> = match low {
            Bound::Included(v) => value_key(v),
            Bound::Excluded(v) => {
                let mut k = value_key(v);
                k.extend_from_slice(&[0xff; 32]);
                k.push(0); // just past every entity of that value
                k
            }
            Bound::Unbounded => Vec::new(),
        };
        let to: Option<Vec<u8>> = match high {
            Bound::Included(v) => {
                let mut k = value_key(v);
                k.extend_from_slice(&[0xff; 32]);
                k.push(0);
                Some(k)
            }
            Bound::Excluded(v) => Some(value_key(v)),
            Bound::Unbounded => None,
        };
        self.scan(attr, ty, &from, to.as_deref())
    }

    // ── Commit ────────────────────────────────────────────────────────────

    /// Apply `changes` on top of this view, writing the new nodes into
    /// `sink`, and return the new roots. This view is unchanged.
    pub fn commit<W: NodeReader<Error = S::Error> + NodeSink>(
        &self,
        sink: &mut W,
        changes: &DbChanges,
    ) -> Result<DbRoots, StoreError<S::Error>> {
        let mut entity_changes = Changes::new();
        // index id -> its trie changes
        let mut index_changes: BTreeMap<Vec<u8>, Changes> = BTreeMap::new();

        for (key, after) in &changes.entities {
            let before = self.entity(key)?;
            entity_changes.insert(key.to_vec(), after.as_ref().map(record::encode));
            if let Some(delta) = annotation_delta(*key, before.as_ref(), after.as_ref()) {
                for AttrEntry { attr, value } in &delta.removes {
                    index_changes
                        .entry(index_id(attr, value.attr_type()))
                        .or_default()
                        .insert(index_entry_key(value, key), None);
                }
                for AttrEntry { attr, value } in &delta.inserts {
                    index_changes
                        .entry(index_id(attr, value.attr_type()))
                        .or_default()
                        .insert(index_entry_key(value, key), Some(INDEX_PRESENT.to_vec()));
                }
            }
        }

        let mut index_root_changes = Changes::new();
        for (id, trie_changes) in &index_changes {
            let old = match Trie::get(sink, self.roots.indexes, id)? {
                Some(bytes) => B256::try_from(bytes.as_slice())
                    .map_err(|_| StoreError::Malformed("index root length"))?,
                None => EMPTY_ROOT_HASH,
            };
            let new = Trie::update(sink, old, trie_changes)?;
            if new != old {
                let value = (new != EMPTY_ROOT_HASH).then(|| new.to_vec());
                index_root_changes.insert(id.clone(), value);
            }
        }

        let nonce_changes: Changes = changes
            .nonces
            .iter()
            .map(|(owner, nonce)| (nonce_key(owner), Some(nonce_value(*nonce))))
            .collect();

        let roots = DbRoots {
            entities: Trie::update(sink, self.roots.entities, &entity_changes)?,
            nonces: Trie::update(sink, self.roots.nonces, &nonce_changes)?,
            indexes: Trie::update(sink, self.roots.indexes, &index_root_changes)?,
        };
        roots.store(sink);
        Ok(roots)
    }
}

/// The smallest byte string greater than every string with `prefix`. `None`
/// when `prefix` is all `0xff` (no upper bound).
fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last < 0xff {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::{Attribute, annotations};
    use arkiv_trie::MemNodeStore;

    fn entity(key: u8, owner: u8, rank: u64, team: &str) -> Entity {
        Entity {
            key: [key; 32],
            creator: [owner; 20],
            owner: [owner; 20],
            created_at_block: 1,
            last_modified_at_block: 1,
            expires_at: 100 + rank,
            content_type: b"text/plain".to_vec(),
            payload: vec![key],
            attributes: vec![
                Attribute::new(b"rank".to_vec(), AttributeValue::U64(rank)),
                Attribute::new(b"team".to_vec(), AttributeValue::Str(team.into())),
            ],
            ..Entity::default()
        }
    }

    fn commit(store: &mut MemNodeStore, roots: DbRoots, changes: DbChanges) -> DbRoots {
        let view = DbView::at(&*store, roots);
        let mut staging = Staging::new(&*store);
        let roots = view.commit(&mut staging, &changes).unwrap();
        for (h, rlp) in staging.into_staged().drain() {
            store.put_node(h, rlp);
        }
        roots
    }

    use arkiv_trie::Staging;

    #[test]
    fn entities_nonces_and_indexes_round_trip() {
        let mut store = MemNodeStore::new();
        let mut changes = DbChanges::default();
        changes
            .entities
            .insert([1; 32], Some(entity(1, 0xaa, 10, "red")));
        changes
            .entities
            .insert([2; 32], Some(entity(2, 0xaa, 20, "blue")));
        changes
            .entities
            .insert([3; 32], Some(entity(3, 0xbb, 30, "red")));
        changes
            .nonces
            .insert([0xaa; 20], EntityCreationNonce::new(2));
        let roots = commit(&mut store, DbRoots::EMPTY, changes);
        let root = roots.root();
        assert_ne!(root, crate::EMPTY_DB_ROOT);

        let view = DbView::open(&store, root).unwrap();
        assert_eq!(
            view.entity(&[2; 32]).unwrap(),
            Some(entity(2, 0xaa, 20, "blue"))
        );
        assert_eq!(view.entity(&[9; 32]).unwrap(), None);
        assert_eq!(view.creation_nonce(&[0xaa; 20]).unwrap().get(), 2);
        assert_eq!(view.creation_nonce(&[0xcc; 20]).unwrap().get(), 0);
        let keys: Vec<_> = view.entity_keys().unwrap().map(Result::unwrap).collect();
        assert_eq!(keys, vec![[1; 32], [2; 32], [3; 32]]);

        let owned = view
            .equal(
                annotations::OWNER,
                &AttributeValue::EthereumAddress([0xaa; 20]),
            )
            .unwrap();
        assert_eq!(owned, vec![[1; 32], [2; 32]]);
        let red = view
            .equal(b"team", &AttributeValue::Str("red".into()))
            .unwrap();
        assert_eq!(red, vec![[1; 32], [3; 32]]);
        let r = view.prefixed(b"team", "r").unwrap();
        assert_eq!(r, vec![[1; 32], [3; 32]]);
        let b = view.prefixed(b"team", "bl").unwrap();
        assert_eq!(b, vec![[2; 32]]);
        let mid = view
            .range(
                b"rank",
                AttributeType::U64,
                Bound::Excluded(&AttributeValue::U64(10)),
                Bound::Included(&AttributeValue::U64(30)),
            )
            .unwrap();
        assert_eq!(mid, vec![[2; 32], [3; 32]]);
        let low = view
            .range(
                b"rank",
                AttributeType::U64,
                Bound::Unbounded,
                Bound::Excluded(&AttributeValue::U64(30)),
            )
            .unwrap();
        assert_eq!(low, vec![[1; 32], [2; 32]]);
        // A different type of the same name is a different index.
        assert!(
            view.equal(b"rank", &AttributeValue::U256([0; 32]))
                .unwrap()
                .is_empty()
        );

        // Update, transfer and delete re-index; the old root is untouched.
        let mut changes = DbChanges::default();
        changes
            .entities
            .insert([1; 32], Some(entity(1, 0xbb, 15, "green")));
        changes.entities.insert([2; 32], None);
        let roots2 = commit(&mut store, roots, changes);
        let view2 = DbView::open(&store, roots2.root()).unwrap();
        assert_eq!(view2.entity(&[2; 32]).unwrap(), None);
        assert_eq!(
            view2
                .equal(
                    annotations::OWNER,
                    &AttributeValue::EthereumAddress([0xbb; 20])
                )
                .unwrap(),
            vec![[1; 32], [3; 32]]
        );
        assert!(
            view2
                .equal(b"team", &AttributeValue::Str("blue".into()))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            view2
                .equal(b"team", &AttributeValue::Str("green".into()))
                .unwrap(),
            vec![[1; 32]]
        );
        let keys: Vec<_> = view2.entity_keys().unwrap().map(Result::unwrap).collect();
        assert_eq!(keys, vec![[1; 32], [3; 32]]);

        let old = DbView::open(&store, root).unwrap();
        assert_eq!(
            old.entity(&[2; 32]).unwrap(),
            Some(entity(2, 0xaa, 20, "blue"))
        );
        assert_eq!(
            old.equal(b"team", &AttributeValue::Str("blue".into()))
                .unwrap(),
            vec![[2; 32]]
        );

        // Deleting everything returns to the empty database root.
        let mut changes = DbChanges::default();
        changes.entities.insert([1; 32], None);
        changes.entities.insert([3; 32], None);
        let roots3 = commit(&mut store, roots2, changes);
        assert_eq!(roots3.entities, EMPTY_ROOT_HASH);
        assert_eq!(roots3.indexes, EMPTY_ROOT_HASH);
        assert_ne!(roots3.nonces, EMPTY_ROOT_HASH);
    }

    #[test]
    fn commit_is_deterministic_in_batch_shape() {
        // One batch of three vs three batches of one give the same root.
        let mut a = MemNodeStore::new();
        let mut changes = DbChanges::default();
        for i in 1..=3u8 {
            changes
                .entities
                .insert([i; 32], Some(entity(i, 1, i as u64, "x")));
        }
        let batched = commit(&mut a, DbRoots::EMPTY, changes).root();

        let mut b = MemNodeStore::new();
        let mut roots = DbRoots::EMPTY;
        for i in [3u8, 1, 2] {
            let mut changes = DbChanges::default();
            changes
                .entities
                .insert([i; 32], Some(entity(i, 1, i as u64, "x")));
            roots = commit(&mut b, roots, changes);
        }
        assert_eq!(batched, roots.root());
    }
}
