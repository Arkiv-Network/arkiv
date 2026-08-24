//! Arkiv's entity executor — the **business logic**, written against the
//! host-agnostic executor interface from [`arkiv_interfaces`].
//!
//! This is the counterpart to the reth plumbing in [`crate`] (the "exact"
//! executor reth injects). Here there is no reth, no revm, no EVM: just the
//! fixed-function entity state transition, expressed over the generic
//! [`StateManager`] / [`Op`] / [`BlockDraft`] vocabulary. Any host that can supply
//! a [`StateManager`] can drive it.
//!
//! ## What it does
//!
//! [`ArkivExecutor::apply`] runs a transaction's operations against the committed
//! [`StateManager`] plus whatever earlier transactions in the same block already
//! staged in the [`BlockDraft`]. It is **all-or-nothing**: operations are staged
//! into a private overlay and only merged into the caller's `draft` if every one
//! succeeds. A business-rule violation (missing entity, wrong owner, …) is a
//! *revert* — [`ExecOutput`] with [`ExecStatus::Reverted`] and the `draft` left
//! untouched — not an [`Err`]. [`Err`] is reserved for host/store faults.
//!
//! The executor works in whole [`Entity`] values throughout; serializing them for
//! storage is the [`StateManager`]'s concern, not this crate's.
//!
//! ## The index delta
//!
//! Besides the entity changes, [`apply`](ArkivExecutor::apply) also stages the
//! matching **query-index** changes into [`draft.auxiliary`](BlockDraft::auxiliary).
//! It does this by *diffing* each touched entity's indexable annotations — the
//! before-the-transaction state against the after — so every op kind is handled
//! uniformly: a create is all-inserts, a delete all-removes, a transfer swaps one
//! `$owner` value, an update touches only what changed. The annotation encoding is
//! the auxiliary store's ([`entity_annotations`]), so the write and read sides agree
//! by construction. Assigning the compact entity ids the index keys on is the
//! store's job, not this one's.
//!
//! ## Effects
//!
//! Decoding raw calldata into [`Op`]s is the host's ABI step (see
//! [`decode`](crate::decode)). On success,
//! [`apply_with_effects`](ArkivExecutor::apply_with_effects) also reports one
//! [`OpEffect`] per op — the key, kind, owner, and expiry the host turns into
//! `EntityOperation` logs. The plain [`apply`](ArkivExecutor::apply) discards them.

use core::fmt;
use core::marker::PhantomData;
use std::collections::BTreeMap;

use arkiv_interfaces::entity::{Attribute, CreationFlags, Entity, annotations};
use arkiv_interfaces::execution::{
    AttributeMutation, BlockDraft, ExecEnv, ExecOutput, ExecStatus, Op, OpKind, RevertReason,
    TransactionExecutor,
};
use arkiv_interfaces::gas::{CostModel, PlaceholderCost};
use arkiv_interfaces::manager::StateManager;
use arkiv_interfaces::primitives::{BlockNumber, EntityAddress, EntityCreationNonce, UserAddress};
use arkiv_interfaces::state::{AttrEntry, AuxiliaryEntityDelta};
use arkiv_reth_mpt_committed_store::indices::annotation::entity_annotations;

/// What a successfully-applied op did — enough for the host to emit its
/// entity-operation log. Host-agnostic (spec types only); the reth wiring turns
/// these into ABI event logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpEffect {
    /// The entity the op targeted.
    pub key: EntityAddress,
    /// Which operation it was.
    pub kind: OpKind,
    /// The entity's owner after the op (the new owner for a transfer; the prior
    /// owner for a delete).
    pub owner: UserAddress,
    /// The entity's expiry after the op.
    pub expires_at: BlockNumber,
    /// The entity's creation flags. Only meaningful for a create — the event it
    /// feeds is the one place they are published.
    pub creation_flags: CreationFlags,
    /// The owner *before* a transfer. `None` for every other op, which have no
    /// ownership change to report.
    pub previous_owner: Option<UserAddress>,
}

/// The fixed-function entity executor.
///
/// Generic over the [`StateManager`] it reads (`S`) and the [`CostModel`] it prices
/// with (`C`, [`PlaceholderCost`] by default). Holds no state itself — a `draft`
/// carries the in-progress block changes.
pub struct ArkivExecutor<S, C = PlaceholderCost> {
    cost: C,
    _state: PhantomData<fn() -> S>,
}

impl<S> ArkivExecutor<S, PlaceholderCost> {
    /// An executor with the zero-cost placeholder schedule.
    pub const fn new() -> Self {
        Self {
            cost: PlaceholderCost,
            _state: PhantomData,
        }
    }
}

impl<S, C> ArkivExecutor<S, C> {
    /// An executor with a specific cost model.
    pub const fn with_cost(cost: C) -> Self {
        Self {
            cost,
            _state: PhantomData,
        }
    }
}

impl<S> Default for ArkivExecutor<S, PlaceholderCost> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, C: Clone> Clone for ArkivExecutor<S, C> {
    fn clone(&self) -> Self {
        Self::with_cost(self.cost.clone())
    }
}

impl<S, C: fmt::Debug> fmt::Debug for ArkivExecutor<S, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArkivExecutor")
            .field("cost", &self.cost)
            .finish()
    }
}

impl<S: StateManager, C: CostModel> TransactionExecutor for ArkivExecutor<S, C> {
    type State = S;
    type Error = ExecError;

    fn execute(
        &self,
        env: &ExecEnv,
        state: &mut Self::State,
        draft: &mut BlockDraft,
        op_bytes: &[u8],
    ) -> Result<ExecOutput, Self::Error> {
        // `start_nonce` is zero here: threading the caller's persistent nonce from the
        // system account is the wiring step's job (this trait path isn't the live
        // reth executor yet). Decode failures are host faults surfaced as errors.
        let ops = crate::decode::decode_ops(env, op_bytes, EntityCreationNonce::ZERO)
            .map_err(|e| ExecError::Decode(e.to_string()))?;
        self.apply(env, state, draft, &ops)
    }
}

impl<S: StateManager, C: CostModel> ArkivExecutor<S, C> {
    /// Run pre-decoded `ops` against `state` + `draft`, all-or-nothing.
    ///
    /// The interpreter counterpart of [`TransactionExecutor::execute`] with the
    /// decode step removed — the real business logic, so it is directly testable.
    pub fn apply(
        &self,
        env: &ExecEnv,
        state: &mut S,
        draft: &mut BlockDraft,
        ops: &[Op],
    ) -> Result<ExecOutput, ExecError> {
        self.apply_with_effects(env, state, draft, ops, &mut Vec::new())
    }

    /// Like [`apply`](Self::apply), but on success also records one [`OpEffect`]
    /// per op into `effects` (in order) — what the host needs to emit the
    /// entity-operation logs. On a revert, `effects` is left untouched.
    pub fn apply_with_effects(
        &self,
        env: &ExecEnv,
        state: &mut S,
        draft: &mut BlockDraft,
        ops: &[Op],
        effects: &mut Vec<OpEffect>,
    ) -> Result<ExecOutput, ExecError> {
        // Per-transaction overlay: Some(entity) = staged write, None = staged
        // delete. Reads consult it before the draft and the store, so operations
        // in this transaction see each other's effects.
        let mut tx_state_overlay = BTreeMap::<EntityAddress, Option<Entity>>::new();
        let mut gas_used = 0u64;
        let mut staged_effects = Vec::with_capacity(ops.len());

        for op in ops {
            // Charge for the op up front. If the batch's cost outruns the gas the
            // caller supplied, it's out of gas: consume everything provided, stage
            // nothing (the overlay is dropped on this early return).
            gas_used = gas_used.saturating_add(self.cost.op_cost(op));
            if gas_used > env.gas_supplied {
                return Ok(ExecOutput {
                    status: ExecStatus::Reverted,
                    gas_used: env.gas_supplied,
                    revert: Some(RevertReason::OutOfGas),
                });
            }
            match self.stage_op(env, state, draft, &mut tx_state_overlay, op)? {
                Ok(effect) => staged_effects.push(effect),
                Err(reason) => {
                    // Business-rule revert: discard the overlay, leave `draft` as it was.
                    return Ok(ExecOutput {
                        status: ExecStatus::Reverted,
                        gas_used,
                        revert: Some(reason),
                    });
                }
            }
        }

        // Every op succeeded. First stage the index changes: for each entity this
        // transaction touched, diff its annotations before vs after. The "before" is
        // read from the draft + store *without* this transaction's overlay, so the
        // diff captures the transaction's net effect on the entity.
        for (key, staged_new) in &tx_state_overlay {
            let before = self.committed_or_drafted(state, draft, *key)?;
            if let Some(entity_delta) = auxiliary_delta(*key, before.as_ref(), staged_new.as_ref())
            {
                draft.auxiliary.entities.push(entity_delta);
            }
        }

        // Then merge the entity overlay into the caller's draft, last-writer-wins per
        // key.
        for (key, staged) in tx_state_overlay {
            draft.entities.puts.retain(|e| e.key != key);
            draft.entities.deletes.retain(|k| *k != key);
            match staged {
                Some(entity) => draft.entities.puts.push(entity),
                None => draft.entities.deletes.push(key),
            }
        }

        effects.extend(staged_effects);
        Ok(ExecOutput {
            status: ExecStatus::Ok,
            gas_used,
            revert: None,
        })
    }

    /// Stage one operation into `overlay`. `Ok(Ok(effect))` staged it and reports
    /// what it did; `Ok(Err(reason))` is a business-rule revert; `Err(_)` is a store
    /// fault.
    fn stage_op(
        &self,
        env: &ExecEnv,
        state: &mut S,
        draft: &BlockDraft,
        overlay: &mut BTreeMap<EntityAddress, Option<Entity>>,
        op: &Op,
    ) -> Result<Result<OpEffect, RevertReason>, ExecError> {
        match op {
            Op::Create {
                key,
                expires_at,
                creation_flags,
                content_type,
                payload,
                attributes,
            } => {
                if self.current(state, draft, overlay, *key)?.is_some() {
                    return Ok(Err(RevertReason::AlreadyExists { key: *key }));
                }
                let entity = Entity {
                    key: *key,
                    creator: env.caller,
                    owner: env.caller,
                    created_at_block: env.block_number,
                    last_modified_at_block: env.block_number,
                    expires_at: *expires_at,
                    creation_flags: *creation_flags,
                    content_type: content_type.clone(),
                    payload: payload.clone(),
                    attributes: attributes.clone(),
                };
                overlay.insert(*key, Some(entity));
                Ok(Ok(OpEffect {
                    key: *key,
                    kind: OpKind::Create,
                    owner: env.caller,
                    expires_at: *expires_at,
                    creation_flags: *creation_flags,
                    previous_owner: None,
                }))
            }
            Op::Patch { key, mutations } => self
                .mutate(
                    env,
                    state,
                    draft,
                    overlay,
                    *key,
                    Auth::OwnerOnly,
                    // `readonly` is fixed at creation and makes contents
                    // immutable — lifecycle ops (extend, transfer, delete)
                    // still work, only the contents are frozen.
                    |e| {
                        if e.creation_flags.is_readonly() {
                            return Err(RevertReason::ReadOnly { key: *key });
                        }
                        Ok(())
                    },
                    |e| apply_mutations(e, mutations),
                )
                .map(|r| {
                    r.map(|(owner, expires_at)| self.effect(*key, OpKind::Patch, owner, expires_at))
                }),
            Op::ExtendExpiry {
                key,
                new_expires_at,
            } => self
                .mutate(
                    env,
                    state,
                    draft,
                    overlay,
                    *key,
                    // `permissionless_extension` lets anyone pay to keep an
                    // entity alive — the one op a non-owner may perform, and
                    // only because extending can't harm the owner.
                    Auth::OwnerOrFlag(CreationFlags::PERMISSIONLESS_EXTENSION),
                    // Lifetimes never shorten. Equal is a no-op rather than a
                    // revert: the resolved target is what the client asked for,
                    // and asking for a lifetime it already has is satisfied.
                    |e| {
                        if *new_expires_at < e.expires_at {
                            return Err(RevertReason::ExpiryNotExtended {
                                key: *key,
                                new_expires_at: *new_expires_at,
                                current_expires_at: e.expires_at,
                            });
                        }
                        Ok(())
                    },
                    |e| {
                        e.expires_at = *new_expires_at;
                    },
                )
                .map(|r| {
                    r.map(|(owner, expires_at)| {
                        self.effect(*key, OpKind::ExtendExpiry, owner, expires_at)
                    })
                }),
            Op::Transfer { key, new_owner } => {
                // Read the outgoing owner before `mutate` overwrites it — the
                // event reports both sides of the handover, and afterwards the
                // old one is gone. `None` here means the entity is missing,
                // which `mutate` reports as `NotFound`.
                let previous_owner = self.current(state, draft, overlay, *key)?.map(|e| e.owner);
                self.mutate(
                    env,
                    state,
                    draft,
                    overlay,
                    *key,
                    Auth::OwnerOnly,
                    // A no-op transfer is a client mistake; matching the contract.
                    |e| {
                        if *new_owner == e.owner {
                            return Err(RevertReason::TransferToSelf { key: *key });
                        }
                        Ok(())
                    },
                    |e| {
                        e.owner = *new_owner;
                    },
                )
                .map(|r| {
                    r.map(|(owner, expires_at)| OpEffect {
                        previous_owner,
                        ..self.effect(*key, OpKind::Transfer, owner, expires_at)
                    })
                })
            }
            Op::Delete { key } => {
                let Some(entity) = self.current(state, draft, overlay, *key)? else {
                    return Ok(Err(RevertReason::NotFound { key: *key }));
                };
                if entity.owner != env.caller {
                    return Ok(Err(RevertReason::NotOwner {
                        key: *key,
                        caller: env.caller,
                        owner: entity.owner,
                    }));
                }
                overlay.insert(*key, None);
                Ok(Ok(self.effect(
                    *key,
                    OpKind::Delete,
                    entity.owner,
                    entity.expires_at,
                )))
            }
        }
    }

    /// Assemble an [`OpEffect`] (a tiny helper so the op arms stay one-liners).
    fn effect(
        &self,
        key: EntityAddress,
        kind: OpKind,
        owner: UserAddress,
        expires_at: BlockNumber,
    ) -> OpEffect {
        OpEffect {
            key,
            kind,
            owner,
            expires_at,
            creation_flags: CreationFlags::NONE,
            previous_owner: None,
        }
    }

    /// Shared read-modify-write for [`Op::Patch`] / [`Op::ExtendExpiry`] /
    /// [`Op::Transfer`]: load the entity, authorize, check liveness, run the
    /// op's own `check` against the loaded entity, mutate, re-stage.
    ///
    /// An expired entity (current block at or past its `expires_at`) is not
    /// mutable — including by [`Op::ExtendExpiry`]: lifetime must be renewed
    /// *before* expiry, not resurrected after it.
    #[allow(clippy::too_many_arguments)]
    fn mutate(
        &self,
        env: &ExecEnv,
        state: &mut S,
        draft: &BlockDraft,
        overlay: &mut BTreeMap<EntityAddress, Option<Entity>>,
        key: EntityAddress,
        auth: Auth,
        check: impl FnOnce(&Entity) -> Result<(), RevertReason>,
        edit: impl FnOnce(&mut Entity),
    ) -> Result<Result<(UserAddress, BlockNumber), RevertReason>, ExecError> {
        let Some(mut entity) = self.current(state, draft, overlay, key)? else {
            return Ok(Err(RevertReason::NotFound { key }));
        };
        if !auth.permits(&entity, &env.caller) {
            return Ok(Err(RevertReason::NotOwner {
                key,
                caller: env.caller,
                owner: entity.owner,
            }));
        }
        if entity.expires_at <= env.block_number {
            return Ok(Err(RevertReason::Expired {
                key,
                expires_at: entity.expires_at,
            }));
        }
        if let Err(reason) = check(&entity) {
            return Ok(Err(reason));
        }
        edit(&mut entity);
        entity.last_modified_at_block = env.block_number;
        let post_op = (entity.owner, entity.expires_at);
        overlay.insert(key, Some(entity));
        Ok(Ok(post_op))
    }

    /// The current entity for `key`: this transaction's overlay first, then the
    /// block's `draft`, then the committed store.
    fn current(
        &self,
        state: &mut S,
        draft: &BlockDraft,
        overlay: &BTreeMap<EntityAddress, Option<Entity>>,
        key: EntityAddress,
    ) -> Result<Option<Entity>, ExecError> {
        if let Some(staged) = overlay.get(&key) {
            return Ok(staged.clone());
        }
        self.committed_or_drafted(state, draft, key)
    }

    /// The entity for `key` as of before this transaction: the block's `draft`
    /// (earlier transactions), then the committed store — **not** this transaction's
    /// overlay. Used to diff the index changes against the pre-transaction state.
    fn committed_or_drafted(
        &self,
        state: &mut S,
        draft: &BlockDraft,
        key: EntityAddress,
    ) -> Result<Option<Entity>, ExecError> {
        if draft.entities.deletes.contains(&key) {
            return Ok(None);
        }
        if let Some(entity) = draft.entities.puts.iter().rev().find(|e| e.key == key) {
            return Ok(Some(entity.clone()));
        }
        state.get(key).map_err(ExecError::store)
    }
}

/// The index change for one entity: the annotations that its `before → after`
/// transition adds and removes. `None` when nothing changed (e.g. a create then
/// delete in the same transaction, which nets to no entity and no index entries).
///
/// The diff is symmetric set difference over the entities' full annotation sets, so
/// Who may perform a mutating op on an entity.
///
/// Ownership is the rule for anything that changes what an entity *is*. The one
/// exception is opened by a creation flag, and only because the op it guards
/// cannot hurt the owner: paying to extend someone else's expiry gives the payer
/// nothing and takes nothing away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// Only the current owner.
    OwnerOnly,
    /// The owner, or anyone at all if the entity carries this creation flag.
    OwnerOrFlag(CreationFlags),
}

impl Auth {
    fn permits(self, entity: &Entity, caller: &UserAddress) -> bool {
        if entity.owner == *caller {
            return true;
        }
        match self {
            Auth::OwnerOnly => false,
            Auth::OwnerOrFlag(flag) => entity.creation_flags.contains(flag),
        }
    }
}

/// Apply a patch's mutations to an entity: set some attributes, unset others,
/// leave everything else untouched.
///
/// This is what makes `patch` a patch. An attribute absent from `mutations`
/// keeps its value, so two patches touching disjoint attributes compose instead
/// of the second clobbering the first — the lost-update race a whole-entity
/// replace has by construction.
///
/// `$payload` and `$contentType` are user-managed system attributes that live in
/// the entity's own fields rather than its attribute list, so they are routed
/// here; unsetting one clears it. Every other `$` name was rejected at decode.
fn apply_mutations(entity: &mut Entity, mutations: &[AttributeMutation]) {
    for m in mutations {
        match m.key.as_slice() {
            annotations::PAYLOAD => {
                entity.payload = m.value.as_ref().map(|v| v.encode()).unwrap_or_default()
            }
            annotations::CONTENT_TYPE => {
                entity.content_type = m.value.as_ref().map(|v| v.encode()).unwrap_or_default()
            }
            name => match &m.value {
                Some(value) => match entity.attributes.iter_mut().find(|a| a.key == name) {
                    // A set replaces the value *and* its type — one name holds
                    // one typed value, so re-typing is a set, not a conflict.
                    Some(existing) => existing.value = value.clone(),
                    None => entity.attributes.push(Attribute {
                        key: name.to_vec(),
                        value: value.clone(),
                    }),
                },
                // Unsetting an attribute that isn't set is a no-op, not an
                // error: the requested end state ("absent") is what results.
                None => entity.attributes.retain(|a| a.key != name),
            },
        }
    }
    // The stored order is canonical, and a push above may have broken it.
    entity.attributes.sort_by(|a, b| a.key.cmp(&b.key));
}

/// it is correct for every op kind without special-casing: whatever the two states
/// disagree on is exactly what the index must change.
fn auxiliary_delta(
    key: EntityAddress,
    before: Option<&Entity>,
    after: Option<&Entity>,
) -> Option<AuxiliaryEntityDelta> {
    let old = before.map(entity_annotations).unwrap_or_default();
    let new = after.map(entity_annotations).unwrap_or_default();

    let contains = |set: &[AttrEntry], entry: &AttrEntry| set.iter().any(|other| other == entry);
    let removes: Vec<AttrEntry> = old.iter().filter(|a| !contains(&new, a)).cloned().collect();
    let inserts: Vec<AttrEntry> = new.iter().filter(|a| !contains(&old, a)).cloned().collect();

    if inserts.is_empty() && removes.is_empty() {
        None
    } else {
        Some(AuxiliaryEntityDelta {
            entity_key: key,
            inserts,
            removes,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::{Attribute, AttributeValue};
    use arkiv_interfaces::manager::{
        AccountBalancesStore, AccountNoncesStore, EntityCreationNoncesStore, PruningMap,
        PruningPriority,
    };
    use arkiv_interfaces::primitives::{Hash, UserBalance, UserNonce};
    use arkiv_interfaces::query::{PageParams, Query, QueryMatches, QueryStats};
    use arkiv_interfaces::state::{
        AuxiliaryStore, BlockAuxiliaryStoreDelta, BlockEntityStoreDelta, EntityStore,
    };
    use core::convert::Infallible;

    /// In-memory [`StateManager`] for the tests. Only the entity lane is
    /// exercised here — the executor reads nothing else — but every lane is
    /// implemented so the double satisfies the same bound a real host does.
    #[derive(Default, Clone)]
    struct MemStore {
        map: BTreeMap<EntityAddress, Entity>,
        balances: BTreeMap<UserAddress, UserBalance>,
        nonces: BTreeMap<UserAddress, UserNonce>,
        entity_nonces: BTreeMap<UserAddress, EntityCreationNonce>,
        pruning: BTreeMap<EntityAddress, (BlockNumber, PruningPriority)>,
    }

    impl EntityStore for MemStore {
        type Error = Infallible;

        fn get(&mut self, entity: EntityAddress) -> Result<Option<Entity>, Infallible> {
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

        fn commitment(&mut self) -> Result<Hash, Infallible> {
            Ok(Default::default())
        }
    }

    impl AuxiliaryStore for MemStore {
        type Error = Infallible;

        fn evaluate(
            &mut self,
            _query: &Query,
            _page: PageParams,
        ) -> Result<QueryMatches, Infallible> {
            Ok(QueryMatches::default())
        }

        fn apply_delta(&mut self, _delta: &BlockAuxiliaryStoreDelta) -> Result<(), Infallible> {
            Ok(())
        }

        fn commitment(&mut self) -> Result<Hash, Infallible> {
            Ok(Default::default())
        }
    }

    impl AccountBalancesStore for MemStore {
        type Error = Infallible;

        fn get_balance(&mut self, account: UserAddress) -> Result<UserBalance, Infallible> {
            Ok(self.balances.get(&account).copied().unwrap_or_default())
        }

        fn set_balance(
            &mut self,
            account: UserAddress,
            balance: UserBalance,
        ) -> Result<(), Infallible> {
            self.balances.insert(account, balance);
            Ok(())
        }
    }

    impl AccountNoncesStore for MemStore {
        type Error = Infallible;

        fn get_account_nonce(&mut self, account: UserAddress) -> Result<UserNonce, Infallible> {
            Ok(self.nonces.get(&account).copied().unwrap_or_default())
        }

        fn set_account_nonce(
            &mut self,
            account: UserAddress,
            nonce: UserNonce,
        ) -> Result<(), Infallible> {
            self.nonces.insert(account, nonce);
            Ok(())
        }
    }

    impl EntityCreationNoncesStore for MemStore {
        type Error = Infallible;

        fn get_entity_nonce(
            &mut self,
            owner: UserAddress,
        ) -> Result<EntityCreationNonce, Infallible> {
            Ok(self.entity_nonces.get(&owner).copied().unwrap_or_default())
        }

        fn advance_entity_nonce(
            &mut self,
            owner: UserAddress,
            by: u64,
        ) -> Result<EntityCreationNonce, Infallible> {
            let slot = self.entity_nonces.entry(owner).or_default();
            let before = *slot;
            *slot = before.advanced_by(by);
            Ok(before)
        }
    }

    impl PruningMap for MemStore {
        type Error = Infallible;

        fn schedule_pruning(
            &mut self,
            entity: EntityAddress,
            prune_at: BlockNumber,
            priority: PruningPriority,
        ) -> Result<(), Infallible> {
            self.pruning.insert(entity, (prune_at, priority));
            Ok(())
        }

        fn pruning_due(&mut self, block: BlockNumber) -> Result<Vec<EntityAddress>, Infallible> {
            let mut due: Vec<_> = self
                .pruning
                .iter()
                .filter(|(_, (at, _))| *at <= block)
                .map(|(key, (_, priority))| (*priority, *key))
                .collect();
            // Highest priority first, ties in ascending key order.
            due.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            Ok(due.into_iter().map(|(_, key)| key).collect())
        }

        fn clear_pruning(&mut self, entities: &[EntityAddress]) -> Result<(), Infallible> {
            for key in entities {
                self.pruning.remove(key);
            }
            Ok(())
        }
    }

    impl StateManager for MemStore {
        type Error = Infallible;

        fn get_operation_cost(&mut self, op: &Op) -> Result<u64, Infallible> {
            Ok(PlaceholderCost.op_cost(op))
        }

        fn get_query_cost(&mut self, stats: &QueryStats) -> Result<u64, Infallible> {
            Ok(PlaceholderCost.query_cost(stats))
        }

        fn get_at(
            &mut self,
            entity: EntityAddress,
            _at: BlockNumber,
        ) -> Result<Option<Entity>, Infallible> {
            EntityStore::get(self, entity)
        }

        fn evaluate_at(
            &mut self,
            query: &Query,
            page: PageParams,
            _at: BlockNumber,
        ) -> Result<QueryMatches, Infallible> {
            AuxiliaryStore::evaluate(self, query, page)
        }
    }

    /// Comfortably more gas than any op in these tests costs, so the state-logic
    /// tests never trip the [`PlaceholderCost`] out-of-gas path.
    const AMPLE_GAS: u64 = 100_000_000;

    /// A minimal [`ExecEnv`] for a given caller and block, with ample gas so the
    /// tests that exercise state logic aren't metered out.
    fn env(caller: [u8; 20], block: u64) -> ExecEnv {
        env_gas(caller, block, AMPLE_GAS)
    }

    /// [`env`] with an explicit gas budget, for the metering tests.
    fn env_gas(caller: [u8; 20], block: u64, gas_supplied: u64) -> ExecEnv {
        ExecEnv {
            caller,
            block_number: block,
            gas_supplied,
            chain_id: 1,
        }
    }

    /// A fully-populated entity (owner `[2; 20]`, `expires_at` 100, two attributes)
    /// used to seed the store in tests that read an existing entity.
    fn sample_entity() -> Entity {
        Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at_block: 3,
            last_modified_at_block: 4,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"hello".to_vec(),
            attributes: vec![
                Attribute::new(b"color".to_vec(), AttributeValue::Str("blue".into())),
                Attribute::new(b"size".to_vec(), AttributeValue::u256_from_u64(1)),
            ],
        }
    }

    /// [`sample_entity`] with a chosen key, for seeding the store.
    fn entity_with_key(key: EntityAddress) -> Entity {
        Entity {
            key,
            ..sample_entity()
        }
    }

    /// A `Create` stages one put whose lifecycle fields come from the environment:
    /// the caller is both `creator` and `owner`, and `created_at_block` is the
    /// block being executed — none of it is taken from the op itself.
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
                    creation_flags: CreationFlags::NONE,
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

    /// `Create` on a key that already exists in the store is a revert, not an
    /// error — and a revert must leave the caller's `draft` exactly as it was
    /// (all-or-nothing: no partial staging leaks out).
    #[test]
    fn create_on_existing_reverts_and_leaves_draft_untouched() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![entity_with_key([1u8; 32])],
                deletes: Vec::new(),
            },
        )
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
                    creation_flags: CreationFlags::NONE,
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

    /// `Transfer` is owner-gated: a caller who is not the current owner reverts
    /// (and stages nothing), while the real owner succeeds — moving `owner` to the
    /// new address and stamping `last_modified_at_block` with the current block.
    #[test]
    fn transfer_requires_ownership_then_moves_owner() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let owner = [2u8; 20];
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()],
                deletes: Vec::new(),
            },
        )
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

    /// A transfer naming the current owner as the new owner reverts
    /// `TransferToSelf` — mirroring the contract's rule.
    #[test]
    fn transfer_to_current_owner_reverts() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let owner = [2u8; 20];
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()],
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let mut draft = BlockDraft::default();

        let out = exec
            .apply(
                &env(owner, 20),
                &mut store,
                &mut draft,
                &[Op::Transfer {
                    key: [7u8; 32],
                    new_owner: owner,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Reverted);
        assert_eq!(
            out.revert,
            Some(RevertReason::TransferToSelf { key: [7u8; 32] })
        );
        assert!(draft.entities.puts.is_empty());
    }

    /// An extend that doesn't move the expiry forward reverts
    /// `ExpiryNotExtended` — lifetime must strictly increase.
    #[test]
    fn extend_never_shortens_and_equal_is_a_no_op() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let owner = [2u8; 20];
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()], // expires_at 100
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let mut draft = BlockDraft::default();

        // Equal expiry succeeds as a no-op: the client asked for a lifetime the
        // entity already has, and that request is satisfied. (This is the rule
        // from arkiv-node-api.md §3 — "> extends, == no-op, < reverts".)
        let out = exec
            .apply(
                &env(owner, 20),
                &mut store,
                &mut draft,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 100,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(draft.entities.puts[0].expires_at, 100);

        // An earlier expiry reverts — lifetimes never shorten.
        let mut shrink_draft = BlockDraft::default();
        let out = exec
            .apply(
                &env(owner, 20),
                &mut store,
                &mut shrink_draft,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 50,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Reverted);
        assert_eq!(
            out.revert,
            Some(RevertReason::ExpiryNotExtended {
                key: [7u8; 32],
                new_expires_at: 50,
                current_expires_at: 100,
            })
        );

        // A later expiry extends.
        let out = exec
            .apply(
                &env(owner, 20),
                &mut store,
                &mut draft,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 150,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(draft.entities.puts[0].expires_at, 150);
    }

    /// Operations within one transaction see each other through the overlay: a
    /// `Delete` following a `Create` of the same key (never committed to the store)
    /// resolves against the just-staged entity, and the net result collapses to a
    /// single staged delete with no put.
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
                        creation_flags: CreationFlags::NONE,
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
        // And no net index change: the entity never existed before and doesn't after.
        assert!(draft.auxiliary.entities.is_empty());
    }

    /// A `Create` stages the entity's whole indexable annotation set as inserts
    /// (seven built-ins + each user attribute) and nothing to remove.
    #[test]
    fn create_stages_index_inserts() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let mut draft = BlockDraft::default();
        let alice = [0xAA; 20];

        exec.apply(
            &env(alice, 10),
            &mut store,
            &mut draft,
            &[Op::Create {
                key: [1u8; 32],
                expires_at: 50,
                creation_flags: CreationFlags::NONE,
                content_type: b"text/plain".to_vec(),
                payload: b"y".to_vec(),
                attributes: vec![Attribute::new(
                    b"rank".to_vec(),
                    AttributeValue::u256_from_u64(0),
                )],
            }],
        )
        .unwrap();

        assert_eq!(draft.auxiliary.entities.len(), 1);
        let delta = &draft.auxiliary.entities[0];
        assert_eq!(delta.entity_key, [1u8; 32]);
        assert!(delta.removes.is_empty());
        assert_eq!(delta.inserts.len(), 8); // 7 built-ins + rank
        // $owner is the caller; the user attribute carries through.
        assert!(
            delta
                .inserts
                .iter()
                .any(|a| a.attr == b"$owner" && a.value == AttributeValue::EthereumAddress(alice))
        );
        assert!(delta.inserts.iter().any(|a| a.attr == b"rank"));
    }

    /// A `Transfer` diffs to a single `$owner` swap — the old owner value out, the
    /// new one in — and touches nothing else (content, other built-ins unchanged).
    #[test]
    fn transfer_stages_only_the_owner_swap() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()], // owner [2; 20]
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let mut draft = BlockDraft::default();

        exec.apply(
            &env([2u8; 20], 20),
            &mut store,
            &mut draft,
            &[Op::Transfer {
                key: [7u8; 32],
                new_owner: [9u8; 20],
            }],
        )
        .unwrap();

        assert_eq!(draft.auxiliary.entities.len(), 1);
        let delta = &draft.auxiliary.entities[0];
        assert_eq!(delta.removes.len(), 1);
        assert_eq!(delta.inserts.len(), 1);
        assert_eq!(delta.removes[0].attr, b"$owner");
        assert_eq!(
            delta.removes[0].value,
            AttributeValue::EthereumAddress([2u8; 20])
        );
        assert_eq!(delta.inserts[0].attr, b"$owner");
        assert_eq!(
            delta.inserts[0].value,
            AttributeValue::EthereumAddress([9u8; 20])
        );
    }

    /// A `Patch` diffs only what changed: the new `$contentType` in, the old one
    /// and the explicitly-unset attribute out. `size`, which the patch never
    /// mentions, does not appear on either side — that is the difference from a
    /// whole-entity replace, which would have dropped it too.
    #[test]
    fn patch_stages_only_changed_annotations() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()], // content text/plain, attrs color + size
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let mut draft = BlockDraft::default();

        exec.apply(
            &env([2u8; 20], 20),
            &mut store,
            &mut draft,
            &[Op::Patch {
                key: [7u8; 32],
                mutations: vec![
                    AttributeMutation::set(
                        annotations::CONTENT_TYPE,
                        AttributeValue::Str("application/json".into()),
                    ),
                    AttributeMutation::unset(b"color".to_vec()),
                ],
            }],
        )
        .unwrap();

        let delta = &draft.auxiliary.entities[0];
        // Out: old $contentType, color. In: new $contentType.
        assert_eq!(delta.removes.len(), 2);
        assert_eq!(delta.inserts.len(), 1);
        assert_eq!(delta.inserts[0].attr, b"$contentType");
        assert_eq!(
            delta.inserts[0].value,
            AttributeValue::Str("application/json".into())
        );
        assert!(delta.removes.iter().any(|a| a.attr == b"color"));
        assert!(
            delta.removes.iter().any(|a| a.attr == b"$contentType"
                && a.value == AttributeValue::Str("text/plain".into()))
        );
        // The untouched attribute is absent from the delta entirely.
        assert!(delta.removes.iter().all(|a| a.attr != b"size"));
        assert!(delta.inserts.iter().all(|a| a.attr != b"size"));
    }

    /// `Update` (owner, live entity) replaces `content_type` / `payload` /
    /// `attributes` and stamps `last_modified_at_block`, while identity and
    /// lifecycle fields (creator, owner, created_at_block, expires_at) are left
    /// exactly as they were.
    #[test]
    fn patch_merges_content_and_stamps_modified() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()], // owner [2; 20], expires_at 100
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let mut draft = BlockDraft::default();

        let out = exec
            .apply(
                &env([2u8; 20], 20),
                &mut store,
                &mut draft,
                &[Op::Patch {
                    key: [7u8; 32],
                    mutations: vec![
                        AttributeMutation::set(
                            annotations::CONTENT_TYPE,
                            AttributeValue::Str("application/json".into()),
                        ),
                        AttributeMutation::set(
                            annotations::PAYLOAD,
                            AttributeValue::Bytes(b"world".to_vec()),
                        ),
                    ],
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        let e = &draft.entities.puts[0];
        assert_eq!(e.content_type, b"application/json");
        assert_eq!(e.payload, b"world");
        // Attributes the patch never mentioned survive — the whole point.
        assert_eq!(e.attributes.len(), 2);
        assert_eq!(e.last_modified_at_block, 20);
        // Identity and lifecycle untouched.
        assert_eq!(e.creator, [1u8; 20]);
        assert_eq!(e.owner, [2u8; 20]);
        assert_eq!(e.created_at_block, 3);
        assert_eq!(e.expires_at, 100);
    }

    /// `ExtendExpiry` (owner, live entity) moves `expires_at` forward and stamps
    /// `last_modified_at_block`; the content is left untouched.
    #[test]
    fn extend_expiry_moves_expiry_and_stamps_modified() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()], // expires_at 100
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let mut draft = BlockDraft::default();

        let out = exec
            .apply(
                &env([2u8; 20], 20),
                &mut store,
                &mut draft,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 500,
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        let e = &draft.entities.puts[0];
        assert_eq!(e.expires_at, 500);
        assert_eq!(e.last_modified_at_block, 20);
        assert_eq!(e.payload, b"hello"); // content untouched
    }

    /// Expiry is final: once the current block reaches `expires_at`, the owner can
    /// no longer mutate the entity — `Update`, `ExtendExpiry`, and `Transfer` all
    /// revert and stage nothing. In particular you cannot resurrect a dead entity
    /// by extending it; renewal must happen before expiry.
    #[test]
    fn cannot_mutate_after_expiry() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        EntityStore::apply_delta(
            &mut store,
            &BlockEntityStoreDelta {
                puts: vec![sample_entity()], // expires_at 100
                deletes: Vec::new(),
            },
        )
        .unwrap();
        let owner = [2u8; 20];

        let expired_ops = [
            Op::Patch {
                key: [7u8; 32],
                mutations: vec![AttributeMutation::set(
                    b"color".to_vec(),
                    AttributeValue::Str("x".into()),
                )],
            },
            Op::ExtendExpiry {
                key: [7u8; 32],
                new_expires_at: 999,
            },
            Op::Transfer {
                key: [7u8; 32],
                new_owner: [9u8; 20],
            },
        ];

        // Block 100 == expires_at, so the entity is expired.
        for op in expired_ops {
            let mut draft = BlockDraft::default();
            let out = exec
                .apply(
                    &env(owner, 100),
                    &mut store,
                    &mut draft,
                    std::slice::from_ref(&op),
                )
                .unwrap();
            assert_eq!(
                out.status,
                ExecStatus::Reverted,
                "{:?} should revert post-expiry",
                op.kind()
            );
            assert!(draft.entities.puts.is_empty());
            assert!(draft.entities.deletes.is_empty());
        }
    }

    /// When a batch's cost exceeds the gas supplied, execution reverts out of gas:
    /// all supplied gas is consumed and nothing is staged. The op's real cost is
    /// taken from [`PlaceholderCost`] so the test tracks the schedule.
    #[test]
    fn reverts_out_of_gas_when_cost_exceeds_supplied() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let mut draft = BlockDraft::default();
        let create = Op::Create {
            key: [1u8; 32],
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            content_type: Vec::new(),
            payload: Vec::new(),
            attributes: Vec::new(),
        };
        let cost = PlaceholderCost.op_cost(&create);
        assert!(cost > 0, "placeholder must charge for a create");

        let out = exec
            .apply(
                &env_gas([0xAA; 20], 10, cost - 1), // one gas short
                &mut store,
                &mut draft,
                core::slice::from_ref(&create),
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Reverted);
        assert_eq!(out.gas_used, cost - 1); // all supplied gas is consumed
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert!(draft.entities.puts.is_empty());
    }

    /// Gas exactly equal to the batch's cost is enough — it succeeds and reports
    /// that cost.
    #[test]
    fn succeeds_when_gas_exactly_covers_cost() {
        let exec = ArkivExecutor::<MemStore>::new();
        let mut store = MemStore::default();
        let mut draft = BlockDraft::default();
        let create = Op::Create {
            key: [1u8; 32],
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            content_type: Vec::new(),
            payload: Vec::new(),
            attributes: Vec::new(),
        };
        let cost = PlaceholderCost.op_cost(&create);

        let out = exec
            .apply(
                &env_gas([0xAA; 20], 10, cost), // exactly enough
                &mut store,
                &mut draft,
                core::slice::from_ref(&create),
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas_used, cost);
        assert_eq!(draft.entities.puts.len(), 1);
    }

    /// Undecodable calldata (here, too short for a selector) surfaces as an
    /// [`ExecError::Decode`] host fault rather than a silent success. The decoder's
    /// own cases are covered in `decode::tests`.
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
