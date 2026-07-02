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
//! The executor works in whole [`Entity`] values throughout; serializing them for
//! storage is the [`EntityStore`]'s concern, not this crate's.
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

use arkiv_interfaces::entity::Entity;
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
            draft.entities.puts.retain(|e| e.key != key);
            draft.entities.deletes.retain(|k| *k != key);
            match staged {
                Some(entity) => draft.entities.puts.push(entity),
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
        if let Some(entity) = draft.entities.puts.iter().rev().find(|e| e.key == key) {
            return Ok(Some(entity.clone()));
        }
        entities.get(key).map_err(ExecError::store)
    }
}

/// Errors that abort execution as a host/store fault (as opposed to a revert,
/// which is a normal [`ExecOutput`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// Raw calldata could not be decoded into operations.
    Decode(String),
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
    use arkiv_interfaces::entity::Attribute;
    use arkiv_interfaces::state::BlockEntityStoreDelta;
    use core::convert::Infallible;

    /// In-memory [`EntityStore`] for the tests.
    #[derive(Default)]
    struct MemStore {
        map: BTreeMap<EntityKey, Entity>,
    }

    impl EntityStore for MemStore {
        type Error = Infallible;

        fn get(&mut self, entity: EntityKey) -> Result<Option<Entity>, Infallible> {
            Ok(self.map.get(&entity).cloned())
        }

        fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Infallible> {
            for entity in &delta.puts {
                self.map.insert(entity.key, entity.clone());
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

    /// [`sample_entity`] with a chosen key, for seeding the store.
    fn entity_with_key(key: EntityKey) -> Entity {
        Entity {
            key,
            ..sample_entity()
        }
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
        let e = &draft.entities.puts[0];
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
                puts: vec![entity_with_key([1u8; 32])],
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
                puts: vec![sample_entity()],
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
        let e = &draft.entities.puts[0];
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
                puts: vec![sample_entity()], // key [7; 32], expires_at = 100
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
