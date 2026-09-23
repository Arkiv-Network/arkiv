//! Arkiv namespaces in a single authenticated ordered map. Full entity keys are
//! retained. Equality, numeric ranges, and string prefixes use the same ordered
//! bitmap buckets; there are no account addresses, slot trees, or string cascades.

use crate::{RecordStore, ScanStats, Tree};
use alloy_primitives::B256;
use arkiv_interfaces::{
    entity::{AttributeType, AttributeValue, Entity, annotations},
    primitives::{EntityAddress, UserAddress},
    query::{PageParams, Query, QueryMatches, QueryStats},
};
use arkiv_reth_mpt_committed_store::{
    AttrEntry, Bitmap, annotation_delta, decode, encode, entity_annotations,
    indices::annotation::{QueryCapabilities, attr_bytes, capabilities_for},
};
use eyre::{Result, ensure, eyre};
use std::ops::{Bound, ControlFlow};

const ENTITY: u8 = 0;
const INDEX: u8 = 1;
const KEY_TO_ID: u8 = 2;
const ID_TO_KEY: u8 = 3;
const NEXT_ID: u8 = 4;
const CREATION_NONCE: u8 = 5;

fn key(namespace: u8, suffix: &[u8]) -> Vec<u8> {
    let mut key = vec![namespace];
    key.extend_from_slice(suffix);
    key
}

fn index_prefix(attribute: &[u8], ty: AttributeType) -> Result<Vec<u8>> {
    let len = u32::try_from(attribute.len())?;
    let mut prefix = vec![INDEX];
    prefix.extend_from_slice(&len.to_be_bytes());
    prefix.extend_from_slice(attribute);
    prefix.push(ty.id());
    Ok(prefix)
}

fn index_key(attribute: &[u8], value: &AttributeValue) -> Result<Vec<u8>> {
    let mut key = index_prefix(attribute, value.attr_type())?;
    key.extend_from_slice(&value.index_bytes());
    Ok(key)
}

fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != u8::MAX {
            end.push(last + 1);
            return end;
        }
    }
    unreachable!("all index prefixes start with INDEX, which is less than 255")
}

fn read_u64(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| eyre!("invalid u64 record"))?,
    ))
}

#[derive(Clone)]
pub struct State<S: RecordStore> {
    tree: Tree<S>,
}

impl<S: RecordStore> State<S> {
    pub fn open(store: S, root: B256) -> Result<Self> {
        Ok(Self {
            tree: Tree::open(store, root)?,
        })
    }

    pub fn root(&self) -> B256 {
        self.tree.root()
    }
    pub fn snapshot(&self) -> Result<crate::Snapshot> {
        self.tree.snapshot()
    }

    pub fn persist(&mut self) -> Result<B256> {
        self.tree.persist()
    }

    /// Roll back the whole operation batch, including IDs, indexes and minting
    /// nonces, if the callback fails. Native account changes are staged by the host.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let root = self.tree.checkpoint();
        let result = f(self);
        self.tree.finish_checkpoint(root, result.is_ok());
        result
    }

    pub fn entity(&self, entity: EntityAddress) -> Result<Option<Entity>> {
        self.tree
            .get(&key(ENTITY, &entity))?
            .map(|bytes| decode(&bytes).map_err(Into::into))
            .transpose()
    }

    fn id(tree: &Tree<S>, entity: EntityAddress) -> Result<Option<u64>> {
        tree.get(&key(KEY_TO_ID, &entity))?
            .map(|bytes| read_u64(&bytes))
            .transpose()
    }

    fn allocate(tree: &mut Tree<S>, entity: EntityAddress) -> Result<u64> {
        if let Some(id) = Self::id(tree, entity)? {
            return Ok(id);
        }
        let id = tree
            .get(&[NEXT_ID])?
            .map(|v| read_u64(&v))
            .transpose()?
            .unwrap_or(0);
        let next = id
            .checked_add(1)
            .ok_or_else(|| eyre!("entity ID overflow"))?;
        tree.insert(vec![NEXT_ID], next.to_be_bytes().to_vec())?;
        tree.insert(key(KEY_TO_ID, &entity), id.to_be_bytes().to_vec())?;
        tree.insert(key(ID_TO_KEY, &id.to_be_bytes()), entity.to_vec())?;
        Ok(id)
    }

    fn bitmap(tree: &Tree<S>, key: &[u8]) -> Result<Bitmap> {
        match tree.get(key)? {
            Some(bytes) => Ok(Bitmap::from_bytes(&bytes)?),
            None => Ok(Bitmap::new()),
        }
    }

    fn update_index(tree: &mut Tree<S>, entries: Vec<AttrEntry>, id: u64, add: bool) -> Result<()> {
        for entry in entries {
            if capabilities_for(&entry.attr, entry.value.attr_type()) == QueryCapabilities::None {
                continue;
            }
            let key = index_key(&entry.attr, &entry.value)?;
            let mut bitmap = Self::bitmap(tree, &key)?;
            if add {
                bitmap.insert(id);
            } else {
                bitmap.remove(id);
            }
            if bitmap.is_empty() {
                tree.remove(&key)?;
            } else {
                tree.insert(key, bitmap.to_bytes())?;
            }
        }
        Ok(())
    }

    /// Storage operation, not business-rule validation. The executor remains
    /// responsible for authorization, expiry, pricing, and minting the entity key.
    pub fn put_entity(&mut self, entity: &Entity) -> Result<()> {
        self.tree.transaction(|tree| {
            let entity_key = key(ENTITY, &entity.key);
            let id = Self::allocate(tree, entity.key)?;
            let before = tree
                .get(&entity_key)?
                .map(|bytes| decode(&bytes))
                .transpose()?;
            if let Some(delta) = annotation_delta(entity.key, before.as_ref(), Some(entity)) {
                Self::update_index(tree, delta.removes, id, false)?;
                Self::update_index(tree, delta.inserts, id, true)?;
            }
            tree.insert(entity_key, encode(entity))
        })
    }

    pub fn remove_entity(&mut self, entity: EntityAddress) -> Result<()> {
        self.tree.transaction(|tree| {
            let entity_key = key(ENTITY, &entity);
            if let Some(before) = tree.get(&entity_key)? {
                let id = Self::id(tree, entity)?.ok_or_else(|| eyre!("entity missing ID"))?;
                Self::update_index(tree, entity_annotations(&decode(&before)?), id, false)?;
                tree.remove(&entity_key)?;
            }
            // IDs remain monotonic, and a reappearing key reuses its old ID.
            Ok(())
        })
    }

    pub fn creation_nonce(&self, owner: UserAddress) -> Result<u64> {
        Ok(self
            .tree
            .get(&key(CREATION_NONCE, &owner))?
            .map(|v| read_u64(&v))
            .transpose()?
            .unwrap_or(0))
    }

    /// Returns the previous nonce, independently of the Ethereum tx nonce.
    pub fn increment_creation_nonce(&mut self, owner: UserAddress) -> Result<u64> {
        let previous = self.creation_nonce(owner)?;
        let next = previous
            .checked_add(1)
            .ok_or_else(|| eyre!("creation nonce overflow"))?;
        self.tree
            .insert(key(CREATION_NONCE, &owner), next.to_be_bytes().to_vec())?;
        Ok(previous)
    }

    pub fn equal(&self, attribute: &[u8], value: &AttributeValue) -> Result<Bitmap> {
        Self::bitmap(&self.tree, &index_key(attribute, value)?)
    }

    /// One bounded traversal, rather than two one-sided scans and intersection.
    /// The caller supplies the type so a fully unbounded typed range is valid.
    pub fn range(
        &self,
        attribute: &[u8],
        ty: AttributeType,
        low: Bound<&AttributeValue>,
        high: Bound<&AttributeValue>,
    ) -> Result<(Bitmap, ScanStats)> {
        ensure!(
            matches!(
                capabilities_for(attribute, ty),
                QueryCapabilities::EqualityAndRange | QueryCapabilities::EqualityAndPrefix
            ),
            "attribute type does not support ranges"
        );
        let prefix = index_prefix(attribute, ty)?;
        let bound_key = |v: &AttributeValue| -> Result<Vec<u8>> {
            ensure!(v.attr_type() == ty, "mixed range types");
            index_key(attribute, v)
        };
        let low = match low {
            Bound::Included(v) => Bound::Included(bound_key(v)?),
            Bound::Excluded(v) => Bound::Excluded(bound_key(v)?),
            Bound::Unbounded => Bound::Included(prefix.clone()),
        };
        let high = match high {
            Bound::Included(v) => Bound::Included(bound_key(v)?),
            Bound::Excluded(v) => Bound::Excluded(bound_key(v)?),
            Bound::Unbounded => Bound::Excluded(prefix_end(&prefix)),
        };
        self.scan_bitmaps(
            low.as_ref().map(Vec::as_slice),
            high.as_ref().map(Vec::as_slice),
        )
    }

    fn scan_bitmaps(&self, low: Bound<&[u8]>, high: Bound<&[u8]>) -> Result<(Bitmap, ScanStats)> {
        let mut hits = Bitmap::new();
        let stats = self.tree.scan(low, high, |_, value| {
            hits.union_with(&Bitmap::from_bytes(value)?);
            Ok(ControlFlow::Continue(()))
        })?;
        Ok((hits, stats))
    }

    pub fn prefix(&self, attribute: &[u8], prefix: &str) -> Result<(Bitmap, ScanStats)> {
        let mut start = index_prefix(attribute, AttributeType::Str)?;
        start.extend_from_slice(prefix.as_bytes());
        let end = prefix_end(&start);
        self.scan_bitmaps(Bound::Included(&start), Bound::Excluded(&end))
    }

    /// Bounded purge selection directly from the authenticated expiration index.
    /// Selecting from the payload's parent root makes this naturally fork-safe.
    pub fn expired(&self, block: u64, limit: usize, gas_limit: u64) -> Result<Vec<B256>> {
        let start = index_prefix(annotations::EXPIRATION, AttributeType::U256)?;
        let end = index_key(
            annotations::EXPIRATION,
            &AttributeValue::u256_from_u64(block),
        )?;
        let mut keys = Vec::new();
        let mut gas = 0u64;
        if limit == 0 {
            return Ok(keys);
        }
        self.tree.scan(
            Bound::Included(&start),
            Bound::Included(&end),
            |_, value| {
                for id in Bitmap::from_bytes(value)?.iter() {
                    let key = self.entity_key(id)?;
                    let entity = self
                        .entity(key)?
                        .ok_or_else(|| eyre!("expiration index references missing entity"))?;
                    let next = gas
                        .saturating_add(arkiv_interfaces::gas::purge_cost(entity.attributes.len()));
                    if next > gas_limit {
                        return Ok(ControlFlow::Break(()));
                    }
                    gas = next;
                    keys.push(B256::from(key));
                    if keys.len() == limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )?;
        Ok(keys)
    }

    pub fn keys(&self, bitmap: &Bitmap) -> Result<Vec<EntityAddress>> {
        let mut keys = bitmap
            .iter()
            .map(|id| self.entity_key(id))
            .collect::<Result<Vec<_>>>()?;
        keys.sort_unstable();
        Ok(keys)
    }

    fn entity_key(&self, id: u64) -> Result<EntityAddress> {
        let bytes = self
            .tree
            .get(&key(ID_TO_KEY, &id.to_be_bytes()))?
            .ok_or_else(|| eyre!("missing entity key for ID {id}"))?;
        bytes.try_into().map_err(|_| eyre!("invalid entity key"))
    }

    pub fn evaluate(&self, query: &Query) -> Result<Bitmap> {
        use Bound::{Excluded, Included, Unbounded};
        match query {
            Query::All => self.equal(annotations::ALL, &AttributeValue::Str(String::new())),
            Query::Eq { key, value } => self.equal(&attr_bytes(key), value),
            Query::Gt { key, value } => self
                .range(
                    &attr_bytes(key),
                    value.attr_type(),
                    Excluded(value),
                    Unbounded,
                )
                .map(|r| r.0),
            Query::Gte { key, value } => self
                .range(
                    &attr_bytes(key),
                    value.attr_type(),
                    Included(value),
                    Unbounded,
                )
                .map(|r| r.0),
            Query::Lt { key, value } => self
                .range(
                    &attr_bytes(key),
                    value.attr_type(),
                    Unbounded,
                    Excluded(value),
                )
                .map(|r| r.0),
            Query::Lte { key, value } => self
                .range(
                    &attr_bytes(key),
                    value.attr_type(),
                    Unbounded,
                    Included(value),
                )
                .map(|r| r.0),
            Query::StartsWith {
                key,
                value: AttributeValue::Str(prefix),
            } => self.prefix(&attr_bytes(key), prefix).map(|r| r.0),
            Query::StartsWith { .. } => Err(eyre!("prefix requires a string")),
            Query::And(l, r) => {
                let mut hits = self.evaluate(l)?;
                if !hits.is_empty() {
                    hits.intersect_with(&self.evaluate(r)?);
                }
                Ok(hits)
            }
            Query::Or(l, r) => {
                let mut hits = self.evaluate(l)?;
                hits.union_with(&self.evaluate(r)?);
                Ok(hits)
            }
            Query::Not(q) => {
                let mut hits = self.evaluate(&Query::All)?;
                hits.subtract(&self.evaluate(q)?);
                Ok(hits)
            }
        }
    }

    /// Existing RPC ordering: descending allocation ID. A range index is ordered
    /// by value, so this still evaluates its full matching bitmap before paging.
    pub fn page(&self, query: &Query, page: PageParams) -> Result<QueryMatches> {
        ensure!(page.page_size > 0, "page size must be positive");
        let matches = self.evaluate(query)?;
        let ids: Vec<_> = matches
            .iter()
            .filter(|id| page.cursor.is_none_or(|cursor| *id < cursor))
            .collect();
        let count = usize::try_from(page.page_size)
            .unwrap_or(usize::MAX)
            .min(ids.len());
        let page_ids: Vec<_> = ids.iter().rev().take(count).copied().collect();
        let keys = page_ids
            .iter()
            .map(|id| self.entity_key(*id))
            .collect::<Result<Vec<_>>>()?;
        Ok(QueryMatches {
            next_cursor: if ids.len() > count {
                page_ids.last().copied()
            } else {
                None
            },
            stats: QueryStats {
                entities_scanned: matches.len(),
                entities_returned: keys.len() as u64,
                index_lookups: page_ids.len() as u64,
                gas_used: 0,
                partial: false,
            },
            keys,
        })
    }
}
