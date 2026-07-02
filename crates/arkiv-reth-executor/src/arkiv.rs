//! Arkiv's entity executor — the **business logic**, written against the
//! host-agnostic executor interface from [`arkiv_interfaces`].
//!
//! This is the counterpart to the reth plumbing in [`crate`] (the "exact"
//! executor reth injects). Here there is no reth, no revm, no EVM: just the
//! fixed-function entity state transition, expressed over the generic
//! [`EntityStore`] / [`Op`] / [`BlockDraft`] vocabulary. Any host that can supply
//! an [`EntityStore`] can drive it.
//!
//! ## What it does
//!
//! [`ArkivExecutor::apply`] runs a transaction's operations against the committed
//! [`EntityStore`] plus whatever earlier transactions in the same block already
//! staged in the [`BlockDraft`]. It is **all-or-nothing**: operations are staged
//! into a private overlay and only merged into the caller's `draft` if every one
//! succeeds. A business-rule violation (missing entity, wrong owner, …) is a
//! *revert* — [`ExecOutput`] with [`ExecStatus::Reverted`] and the `draft` left
//! untouched — not an [`Err`]. [`Err`] is reserved for host/store faults.
//!
//! ## Record encoding
//!
//! The [`EntityStore`] holds opaque bytes; turning an [`Entity`] into those bytes
//! is the executor's job. [`encode_entity`] / [`decode_entity`] are a small,
//! self-contained, versioned format for exactly that. It is private to the
//! executor (not a wire ABI) and can change independently.
//!
//! ## Not yet wired
//!
//! Decoding raw transaction calldata into [`Op`]s is the host's ABI step (see the
//! interface docs); [`decode_ops`] is a placeholder until that lands. The business
//! logic in [`ArkivExecutor::apply`] is complete and tested against pre-decoded
//! operations.

use core::fmt;
use core::marker::PhantomData;
use std::collections::BTreeMap;

use arkiv_interfaces::entity::{Attribute, Entity};
use arkiv_interfaces::execution::{
    BlockDraft, ExecEnv, ExecOutput, ExecStatus, Op, TransactionExecutor,
};
use arkiv_interfaces::gas::{CostModel, PlaceholderCost};
use arkiv_interfaces::primitives::EntityKey;
use arkiv_interfaces::state::EntityStore;

/// The fixed-function entity executor.
///
/// Generic over the [`EntityStore`] it reads (`E`) and the [`CostModel`] it prices
/// with (`C`, [`PlaceholderCost`] by default). Holds no state itself — a `draft`
/// carries the in-progress block changes.
pub struct ArkivExecutor<E, C = PlaceholderCost> {
    cost: C,
    _store: PhantomData<fn() -> E>,
}

impl<E> ArkivExecutor<E, PlaceholderCost> {
    /// An executor with the zero-cost placeholder schedule.
    pub const fn new() -> Self {
        Self {
            cost: PlaceholderCost,
            _store: PhantomData,
        }
    }
}

impl<E, C> ArkivExecutor<E, C> {
    /// An executor with a specific cost model.
    pub const fn with_cost(cost: C) -> Self {
        Self {
            cost,
            _store: PhantomData,
        }
    }
}

impl<E> Default for ArkivExecutor<E, PlaceholderCost> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E, C: Clone> Clone for ArkivExecutor<E, C> {
    fn clone(&self) -> Self {
        Self::with_cost(self.cost.clone())
    }
}

impl<E, C: fmt::Debug> fmt::Debug for ArkivExecutor<E, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArkivExecutor")
            .field("cost", &self.cost)
            .finish()
    }
}

impl<E: EntityStore, C: CostModel> TransactionExecutor for ArkivExecutor<E, C> {
    type Entities = E;
    type Error = ExecError;

    fn execute(
        &self,
        env: &ExecEnv,
        entities: &mut Self::Entities,
        draft: &mut BlockDraft,
        op_bytes: &[u8],
    ) -> Result<ExecOutput, Self::Error> {
        let ops = decode_ops(op_bytes)?;
        self.apply(env, entities, draft, &ops)
    }
}

impl<E: EntityStore, C: CostModel> ArkivExecutor<E, C> {
    /// Run pre-decoded `ops` against `entities` + `draft`, all-or-nothing.
    ///
    /// The interpreter counterpart of [`TransactionExecutor::execute`] with the
    /// decode step removed — the real business logic, so it is directly testable.
    pub fn apply(
        &self,
        env: &ExecEnv,
        entities: &mut E,
        draft: &mut BlockDraft,
        ops: &[Op],
    ) -> Result<ExecOutput, ExecError> {
        // Per-transaction overlay: Some(entity) = staged write, None = staged
        // delete. Reads consult it before the draft and the store, so operations
        // in this transaction see each other's effects.
        let mut overlay: BTreeMap<EntityKey, Option<Entity>> = BTreeMap::new();
        let mut gas_used = 0u64;

        for op in ops {
            gas_used = gas_used.saturating_add(self.cost.op_cost(op));
            if let Err(reason) = self.stage_op(env, entities, draft, &mut overlay, op)? {
                // Business-rule revert: discard the overlay, leave `draft` as it was.
                return Ok(ExecOutput {
                    status: ExecStatus::Reverted,
                    gas_used,
                    revert: Some(reason),
                });
            }
        }

        // Every op succeeded — merge the overlay into the caller's draft,
        // last-writer-wins per key.
        for (key, staged) in overlay {
            draft.entities.puts.retain(|(k, _)| *k != key);
            draft.entities.deletes.retain(|k| *k != key);
            match staged {
                Some(entity) => draft.entities.puts.push((key, encode_entity(&entity))),
                None => draft.entities.deletes.push(key),
            }
        }

        Ok(ExecOutput {
            status: ExecStatus::Ok,
            gas_used,
            revert: None,
        })
    }

    /// Stage one operation into `overlay`. `Ok(Ok(()))` staged it; `Ok(Err(reason))`
    /// is a business-rule revert; `Err(_)` is a store fault.
    fn stage_op(
        &self,
        env: &ExecEnv,
        entities: &mut E,
        draft: &BlockDraft,
        overlay: &mut BTreeMap<EntityKey, Option<Entity>>,
        op: &Op,
    ) -> Result<Result<(), String>, ExecError> {
        match op {
            Op::Create {
                key,
                expires_at,
                content_type,
                payload,
                attributes,
            } => {
                if self.current(entities, draft, overlay, *key)?.is_some() {
                    return Ok(Err(format!("entity {} already exists", hex(key))));
                }
                let entity = Entity {
                    key: *key,
                    creator: env.caller,
                    owner: env.caller,
                    created_at_block: env.block_number,
                    last_modified_at_block: env.block_number,
                    expires_at: *expires_at,
                    content_type: content_type.clone(),
                    payload: payload.clone(),
                    attributes: attributes.clone(),
                };
                overlay.insert(*key, Some(entity));
                Ok(Ok(()))
            }
            Op::Update {
                key,
                content_type,
                payload,
                attributes,
            } => self.mutate(env, entities, draft, overlay, *key, |e| {
                e.content_type = content_type.clone();
                e.payload = payload.clone();
                e.attributes = attributes.clone();
            }),
            Op::Extend {
                key,
                new_expires_at,
            } => self.mutate(env, entities, draft, overlay, *key, |e| {
                e.expires_at = *new_expires_at;
            }),
            Op::Transfer { key, new_owner } => {
                self.mutate(env, entities, draft, overlay, *key, |e| {
                    e.owner = *new_owner;
                })
            }
            Op::Delete { key } => {
                let Some(entity) = self.current(entities, draft, overlay, *key)? else {
                    return Ok(Err(format!("entity {} does not exist", hex(key))));
                };
                if entity.owner != env.caller {
                    return Ok(Err(format!("caller does not own entity {}", hex(key))));
                }
                overlay.insert(*key, None);
                Ok(Ok(()))
            }
            Op::Expire { key } => {
                let Some(entity) = self.current(entities, draft, overlay, *key)? else {
                    return Ok(Err(format!("entity {} does not exist", hex(key))));
                };
                if entity.expires_at > env.block_number {
                    return Ok(Err(format!("entity {} has not expired", hex(key))));
                }
                overlay.insert(*key, None);
                Ok(Ok(()))
            }
        }
    }

    /// Shared read-modify-write for [`Op::Update`] / [`Op::Extend`] /
    /// [`Op::Transfer`]: load the entity, check ownership, mutate, re-stage.
    fn mutate(
        &self,
        env: &ExecEnv,
        entities: &mut E,
        draft: &BlockDraft,
        overlay: &mut BTreeMap<EntityKey, Option<Entity>>,
        key: EntityKey,
        edit: impl FnOnce(&mut Entity),
    ) -> Result<Result<(), String>, ExecError> {
        let Some(mut entity) = self.current(entities, draft, overlay, key)? else {
            return Ok(Err(format!("entity {} does not exist", hex(&key))));
        };
        if entity.owner != env.caller {
            return Ok(Err(format!("caller does not own entity {}", hex(&key))));
        }
        edit(&mut entity);
        entity.last_modified_at_block = env.block_number;
        overlay.insert(key, Some(entity));
        Ok(Ok(()))
    }

    /// The current entity for `key`: this transaction's overlay first, then the
    /// block's `draft`, then the committed store.
    fn current(
        &self,
        entities: &mut E,
        draft: &BlockDraft,
        overlay: &BTreeMap<EntityKey, Option<Entity>>,
        key: EntityKey,
    ) -> Result<Option<Entity>, ExecError> {
        if let Some(staged) = overlay.get(&key) {
            return Ok(staged.clone());
        }
        if draft.entities.deletes.contains(&key) {
            return Ok(None);
        }
        if let Some((_, bytes)) = draft.entities.puts.iter().rev().find(|(k, _)| *k == key) {
            return Ok(Some(decode_entity(bytes)?));
        }
        match entities.get(key).map_err(ExecError::store)? {
            Some(bytes) => Ok(Some(decode_entity(&bytes)?)),
            None => Ok(None),
        }
    }
}

/// Errors that abort execution as a host/store fault (as opposed to a revert,
/// which is a normal [`ExecOutput`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// Raw calldata could not be decoded into operations.
    Decode(String),
    /// A stored record's bytes were malformed.
    Codec(String),
    /// The underlying [`EntityStore`] returned an error.
    Store(String),
}

impl ExecError {
    fn store<E: fmt::Debug>(e: E) -> Self {
        ExecError::Store(format!("{e:?}"))
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Decode(m) => write!(f, "op decode error: {m}"),
            ExecError::Codec(m) => write!(f, "record codec error: {m}"),
            ExecError::Store(m) => write!(f, "store error: {m}"),
        }
    }
}

impl std::error::Error for ExecError {}

/// Decode raw transaction calldata into operations.
///
/// **Placeholder.** The real decoder is the host's ABI step (the `Operation[]`
/// layout from the Arkiv contracts); it lands with the reth bridge. Until then
/// empty calldata is an empty batch and anything else is rejected, so the seam is
/// explicit rather than silently wrong.
fn decode_ops(op_bytes: &[u8]) -> Result<Vec<Op>, ExecError> {
    if op_bytes.is_empty() {
        Ok(Vec::new())
    } else {
        Err(ExecError::Decode(
            "op-batch ABI decoding is not wired yet".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Record codec — the executor's private on-store encoding of an Entity.
// ---------------------------------------------------------------------------

/// Current record format version.
const RECORD_V1: u8 = 1;

/// Encode an [`Entity`] to the bytes an [`EntityStore`] holds.
///
/// Format v1, all integers big-endian: `version(1) | key(32) | creator(20) |
/// owner(20) | created(8) | last_modified(8) | expires(8) | len(content_type) |
/// content_type | len(payload) | payload | count(attrs) | attrs…` where each
/// attribute is `len(key) | key | value_type(1) | len(value) | value` and every
/// `len`/`count` is a big-endian `u32`.
pub fn encode_entity(e: &Entity) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(RECORD_V1);
    out.extend_from_slice(&e.key);
    out.extend_from_slice(&e.creator);
    out.extend_from_slice(&e.owner);
    out.extend_from_slice(&e.created_at_block.to_be_bytes());
    out.extend_from_slice(&e.last_modified_at_block.to_be_bytes());
    out.extend_from_slice(&e.expires_at.to_be_bytes());
    put_bytes(&mut out, &e.content_type);
    put_bytes(&mut out, &e.payload);
    out.extend_from_slice(&(e.attributes.len() as u32).to_be_bytes());
    for attr in &e.attributes {
        put_bytes(&mut out, &attr.key);
        out.push(attr.value_type);
        put_bytes(&mut out, &attr.value);
    }
    out
}

/// Decode an [`Entity`] from the bytes an [`EntityStore`] holds. Inverse of
/// [`encode_entity`]; returns [`ExecError::Codec`] on malformed input.
pub fn decode_entity(bytes: &[u8]) -> Result<Entity, ExecError> {
    let mut r = Reader::new(bytes);
    match r.u8()? {
        RECORD_V1 => {}
        v => return Err(ExecError::Codec(format!("unknown record version {v}"))),
    }
    let key = r.array::<32>()?;
    let creator = r.array::<20>()?;
    let owner = r.array::<20>()?;
    let created_at_block = u64::from_be_bytes(r.array::<8>()?);
    let last_modified_at_block = u64::from_be_bytes(r.array::<8>()?);
    let expires_at = u64::from_be_bytes(r.array::<8>()?);
    let content_type = r.bytes()?;
    let payload = r.bytes()?;
    let n_attrs = u32::from_be_bytes(r.array::<4>()?) as usize;
    let mut attributes = Vec::with_capacity(n_attrs);
    for _ in 0..n_attrs {
        let key = r.bytes()?;
        let value_type = r.u8()?;
        let value = r.bytes()?;
        attributes.push(Attribute {
            key,
            value_type,
            value,
        });
    }
    if !r.is_empty() {
        return Err(ExecError::Codec("trailing bytes in record".to_string()));
    }
    Ok(Entity {
        key,
        creator,
        owner,
        created_at_block,
        last_modified_at_block,
        expires_at,
        content_type,
        payload,
        attributes,
    })
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

/// A bounds-checked cursor over the record bytes.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ExecError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.buf.len())
            .ok_or_else(|| ExecError::Codec("record ends mid-field".to_string()))?;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ExecError> {
        Ok(self.take(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ExecError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ExecError> {
        let len = u32::from_be_bytes(self.array::<4>()?) as usize;
        Ok(self.take(len)?.to_vec())
    }
}

/// Lower-hex of a key, for revert messages.
fn hex(key: &EntityKey) -> String {
    let mut s = String::with_capacity(2 + key.len() * 2);
    s.push_str("0x");
    for b in key {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::state::BlockEntityStoreDelta;
    use core::convert::Infallible;

    /// In-memory [`EntityStore`] for the tests.
    #[derive(Default)]
    struct MemStore {
        map: BTreeMap<EntityKey, Vec<u8>>,
    }

    impl EntityStore for MemStore {
        type Error = Infallible;

        fn get(&mut self, entity: EntityKey) -> Result<Option<Vec<u8>>, Infallible> {
            Ok(self.map.get(&entity).cloned())
        }

        fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Infallible> {
            for (k, v) in &delta.puts {
                self.map.insert(*k, v.clone());
            }
            for k in &delta.deletes {
                self.map.remove(k);
            }
            Ok(())
        }

        fn commitment(&mut self) -> Result<arkiv_interfaces::primitives::Hash, Infallible> {
            Ok(Default::default())
        }
    }

    fn env(caller: [u8; 20], block: u64) -> ExecEnv {
        ExecEnv {
            caller,
            block_number: block,
            gas_supplied: 0,
            chain_id: 1,
        }
    }

    fn sample_entity() -> Entity {
        Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 3,
            last_modified_at_block: 4,
            expires_at: 100,
            content_type: b"text/plain".to_vec(),
            payload: b"hello".to_vec(),
            attributes: vec![
                Attribute {
                    key: b"color".to_vec(),
                    value_type: arkiv_interfaces::entity::ATTR_STRING,
                    value: b"blue".to_vec(),
                },
                Attribute {
                    key: b"size".to_vec(),
                    value_type: arkiv_interfaces::entity::ATTR_UINT,
                    value: vec![0u8; 32],
                },
            ],
        }
    }

    #[test]
    fn record_codec_roundtrips() {
        let e = sample_entity();
        assert_eq!(decode_entity(&encode_entity(&e)).unwrap(), e);
    }

    #[test]
    fn decode_rejects_truncated_and_trailing() {
        let bytes = encode_entity(&sample_entity());
        assert!(matches!(
            decode_entity(&bytes[..bytes.len() - 1]),
            Err(ExecError::Codec(_))
        ));
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(decode_entity(&extra), Err(ExecError::Codec(_))));
    }

    #[test]
    fn create_stages_a_put() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let mut draft = BlockDraft::default();
        let alice = [0xAA; 20];

        let out = exec
            .apply(
                &env(alice, 10),
                &mut store,
                &mut draft,
                &[Op::Create {
                    key: [1u8; 32],
                    expires_at: 50,
                    content_type: b"x".to_vec(),
                    payload: b"y".to_vec(),
                    attributes: Vec::new(),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(draft.entities.puts.len(), 1);
        let e = decode_entity(&draft.entities.puts[0].1).unwrap();
        assert_eq!(e.owner, alice);
        assert_eq!(e.creator, alice);
        assert_eq!(e.created_at_block, 10);
    }

    #[test]
    fn create_on_existing_reverts_and_leaves_draft_untouched() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: vec![([1u8; 32], encode_entity(&sample_entity()))],
                deletes: Vec::new(),
            })
            .unwrap();
        let mut draft = BlockDraft::default();

        let out = exec
            .apply(
                &env([0xAA; 20], 10),
                &mut store,
                &mut draft,
                &[Op::Create {
                    key: [1u8; 32],
                    expires_at: 50,
                    content_type: Vec::new(),
                    payload: Vec::new(),
                    attributes: Vec::new(),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Reverted);
        assert!(out.revert.is_some());
        assert!(draft.entities.puts.is_empty());
    }

    #[test]
    fn transfer_requires_ownership_then_moves_owner() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let owner = [2u8; 20];
        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: vec![([7u8; 32], encode_entity(&sample_entity()))],
                deletes: Vec::new(),
            })
            .unwrap();
        let mut draft = BlockDraft::default();

        // Wrong caller reverts.
        let out = exec
            .apply(
                &env([0xFF; 20], 20),
                &mut store,
                &mut draft,
                &[Op::Transfer {
                    key: [7u8; 32],
                    new_owner: [9u8; 20],
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Reverted);
        assert!(draft.entities.puts.is_empty());

        // Real owner succeeds and the new owner is staged.
        let out = exec
            .apply(
                &env(owner, 20),
                &mut store,
                &mut draft,
                &[Op::Transfer {
                    key: [7u8; 32],
                    new_owner: [9u8; 20],
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        let e = decode_entity(&draft.entities.puts[0].1).unwrap();
        assert_eq!(e.owner, [9u8; 20]);
        assert_eq!(e.last_modified_at_block, 20);
    }

    #[test]
    fn create_then_delete_in_one_tx_sees_overlay() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let mut draft = BlockDraft::default();
        let alice = [0xAA; 20];

        let out = exec
            .apply(
                &env(alice, 10),
                &mut store,
                &mut draft,
                &[
                    Op::Create {
                        key: [3u8; 32],
                        expires_at: 50,
                        content_type: Vec::new(),
                        payload: Vec::new(),
                        attributes: Vec::new(),
                    },
                    Op::Delete { key: [3u8; 32] },
                ],
            )
            .unwrap();

        // Net effect: created then deleted → a single staged delete, no put.
        assert_eq!(out.status, ExecStatus::Ok);
        assert!(draft.entities.puts.is_empty());
        assert_eq!(draft.entities.deletes, vec![[3u8; 32]]);
    }

    #[test]
    fn expire_only_after_expiry_block() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        store
            .apply_delta(&BlockEntityStoreDelta {
                puts: vec![([7u8; 32], encode_entity(&sample_entity()))], // expires_at = 100
                deletes: Vec::new(),
            })
            .unwrap();

        let mut draft = BlockDraft::default();
        let out = exec
            .apply(
                &env([0u8; 20], 50),
                &mut store,
                &mut draft,
                &[Op::Expire { key: [7u8; 32] }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Reverted);

        let mut draft = BlockDraft::default();
        let out = exec
            .apply(
                &env([0u8; 20], 100),
                &mut store,
                &mut draft,
                &[Op::Expire { key: [7u8; 32] }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(draft.entities.deletes, vec![[7u8; 32]]);
    }

    #[test]
    fn execute_rejects_undecodable_calldata() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let mut draft = BlockDraft::default();
        assert!(matches!(
            exec.execute(&env([0u8; 20], 1), &mut store, &mut draft, &[0xDE, 0xAD]),
            Err(ExecError::Decode(_))
        ));
    }
}
