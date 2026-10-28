//! Arkiv's entity executor — the business logic, host-agnostic. All-or-nothing:
//! a batch runs in one frame on the view's branch, each op writing as it runs,
//! and only a batch whose every op succeeds keeps its writes. A business-rule
//! violation is a *revert*, not an [`Err`] — errors are host/store faults.

use core::fmt;
use core::marker::PhantomData;

use arkiv_interfaces::constants::ENTITY_MAX_ATTRIBUTES;
use arkiv_interfaces::entity::{Attribute, CreationFlags, Entity};
use arkiv_interfaces::entity_records;
use arkiv_interfaces::execution::{
    AttributeMutation, ExecEnv, ExecOutput, ExecStatus, Op, OpKind, RevertReason,
    TransactionExecutor,
};
use arkiv_interfaces::gas::{CostModel, GasBreakdown, PlaceholderCost};
use arkiv_interfaces::primitives::{
    BlockNumber, EntityAddress, EntityCreationNonce, Gas, UserAddress,
};
use arkiv_interfaces::statemanager::{EntityUpdates, ReadMode, StateView, StoredEntity};
use arkiv_interfaces::store::{CostUnits, Metered, Receipt, RecordMeta, WriteDetails};
use arkiv_tokenomics::{FeeSchedule, GasMeter, Overflow, storage_time, units};

/// What a successfully-applied op did — enough for the host to emit its
/// entity-operation log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpEffect {
    pub key: EntityAddress,
    pub kind: OpKind,
    /// The owner after the op.
    pub owner: UserAddress,
    pub expires_at: BlockNumber,
    /// Only meaningful for a create.
    pub creation_flags: CreationFlags,
    /// The owner before a transfer; `None` for every other op.
    pub previous_owner: Option<UserAddress>,
}

pub struct ArkivExecutor<V, C = PlaceholderCost> {
    cost: C,
    /// The block's fee schedule, which storage is priced with.
    schedule: FeeSchedule,
    _state: PhantomData<fn() -> V>,
}

/// The schedule an executor prices with until it is given the block's: the
/// placeholder weights. Its `max_lifetime` is not consulted; the lifetime
/// bound comes in [`ExecEnv::max_lifetime`].
const DEFAULT_SCHEDULE: FeeSchedule = FeeSchedule::with_placeholder_weights(BlockNumber::MAX);

impl<V> ArkivExecutor<V, PlaceholderCost> {
    pub const fn new() -> Self {
        Self::with_cost(PlaceholderCost)
    }
}

impl<V, C> ArkivExecutor<V, C> {
    pub const fn with_cost(cost: C) -> Self {
        Self {
            cost,
            schedule: DEFAULT_SCHEDULE,
            _state: PhantomData,
        }
    }

    /// Price with the block's schedule, read at its base. Interim until
    /// GolemDB's pricing snapshot carries it.
    pub const fn with_schedule(mut self, schedule: FeeSchedule) -> Self {
        self.schedule = schedule;
        self
    }
}

impl<V> Default for ArkivExecutor<V, PlaceholderCost> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V, C: Clone> Clone for ArkivExecutor<V, C> {
    fn clone(&self) -> Self {
        Self::with_cost(self.cost.clone()).with_schedule(self.schedule)
    }
}

impl<V, C: fmt::Debug> fmt::Debug for ArkivExecutor<V, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArkivExecutor")
            .field("cost", &self.cost)
            .field("schedule", &self.schedule)
            .finish()
    }
}

impl<V: StateView, C: CostModel> TransactionExecutor for ArkivExecutor<V, C> {
    type State = V;
    type Error = ExecError;

    fn execute(
        &self,
        env: &ExecEnv,
        state: &mut Self::State,
        op_bytes: &[u8],
    ) -> Result<ExecOutput, Self::Error> {
        // `start_nonce` is zero here: threading the caller's persistent nonce is
        // the wiring step's job. Decode failures are host faults.
        let ops = crate::decode::decode_ops(env, op_bytes, EntityCreationNonce::ZERO)
            .map_err(|e| ExecError::Decode(e.to_string()))?;
        self.apply(env, state, &ops)
    }
}

impl<V: StateView, C: CostModel> ArkivExecutor<V, C> {
    /// Run pre-decoded `ops` against `state`, all-or-nothing.
    pub fn apply(&self, env: &ExecEnv, state: &mut V, ops: &[Op]) -> Result<ExecOutput, ExecError> {
        self.apply_with_effects(env, state, ops, &mut Vec::new())
    }

    /// Like [`apply`](Self::apply), but on success also records one
    /// [`OpEffect`] per op into `effects`. On a revert, `effects` is untouched.
    ///
    /// The batch runs in one frame on the view's branch: each op writes as it
    /// runs, so later ops read earlier ones' effects through the branch. A
    /// revert discards the frame, and so does a batch that does not keep its
    /// writes ([`ExecEnv::keep_writes`]); otherwise the frame is merged. A
    /// store fault discards it too, before it is returned.
    pub fn apply_with_effects(
        &self,
        env: &ExecEnv,
        state: &mut V,
        ops: &[Op],
        effects: &mut Vec<OpEffect>,
    ) -> Result<ExecOutput, ExecError> {
        state.begin_frame().map_err(ExecError::store)?;
        let mut staged_effects = Vec::with_capacity(ops.len());
        let out = match self.run_batch(env, state, ops, &mut staged_effects) {
            Ok(out) => out,
            Err(fault) => {
                // The fault is what the caller needs; a second one from the
                // rollback would only hide it. Nothing builds on a branch a
                // fault ran on: it aborts the whole block, whose branch is then
                // abandoned, so a failed rollback leaves nothing behind.
                let _ = state.discard_frame();
                return Err(fault);
            }
        };
        let keep = out.status == ExecStatus::Ok && env.keep_writes;
        if keep {
            state.merge_frame()
        } else {
            state.discard_frame()
        }
        .map_err(ExecError::store)?;
        if out.status == ExecStatus::Ok {
            effects.extend(staged_effects);
        }
        Ok(out)
    }

    /// Run `ops` in order, each metered and staged on the branch. The first
    /// revert ends the batch; the caller's frame undoes its writes, but not
    /// what they cost.
    ///
    /// Each op's own cost is checked against the supplied gas before it
    /// writes, and everything it spent after it ran. Running out, or a sum
    /// that does not fit, consumes all the gas supplied.
    fn run_batch(
        &self,
        env: &ExecEnv,
        state: &mut V,
        ops: &[Op],
        effects: &mut Vec<OpEffect>,
    ) -> Result<ExecOutput, ExecError> {
        let mut meter = BatchMeter::starting_at(env.golemdb_cost_before);
        for op in ops {
            let affordable = meter
                .gas
                .add_op_cost(self.cost.op_cost(op))
                .and_then(|()| meter.gas.compute_so_far(env.tx_gas))
                .is_ok_and(|compute| compute <= env.gas_supplied);
            if !affordable {
                return Ok(out_of_gas(env));
            }
            let staged = self.stage_op(env, state, &mut meter, op)?;
            if meter.overflowed {
                return Ok(out_of_gas(env));
            }
            match staged {
                Ok(effect) => effects.push(effect),
                Err(reason) => return Ok(self.reverted(env, &meter, reason)),
            }
            if self.spent(env, &meter).is_none() {
                return Ok(out_of_gas(env));
            }
        }
        Ok(match self.spent(env, &meter) {
            Some(gas) => ExecOutput {
                status: ExecStatus::Ok,
                gas,
                revert: None,
            },
            None => out_of_gas(env),
        })
    }

    /// What the batch has spent, if it still fits the supplied gas.
    fn spent(&self, env: &ExecEnv, meter: &BatchMeter) -> Option<GasBreakdown> {
        meter
            .gas
            .finish(env.tx_gas, env.floor_gas, &self.schedule)
            .ok()
            .filter(|gas| gas.total().is_some_and(|total| total <= env.gas_supplied))
    }

    /// A business-rule revert pays the compute it spent, GolemDB's work on
    /// writes now undone included: GolemDB charges rolled-back work. It buys
    /// no storage, because the frame keeps none of it.
    fn reverted(&self, env: &ExecEnv, meter: &BatchMeter, reason: RevertReason) -> ExecOutput {
        if reason == RevertReason::OutOfGas {
            return out_of_gas(env);
        }
        let Ok(compute) = meter.gas.compute_so_far(env.tx_gas) else {
            return out_of_gas(env);
        };
        ExecOutput {
            status: ExecStatus::Reverted,
            gas: GasBreakdown {
                compute_gas: compute.max(env.floor_gas).min(env.gas_supplied),
                storage_gas: 0,
                purge_gas: 0,
            },
            revert: Some(reason),
        }
    }

    /// Stage one operation on the view's branch. `Ok(Ok(effect))` staged it;
    /// `Ok(Err(reason))` is a business-rule revert, after which the caller
    /// discards whatever earlier ops wrote; `Err(_)` a store fault.
    fn stage_op(
        &self,
        env: &ExecEnv,
        state: &mut V,
        meter: &mut BatchMeter,
        op: &Op,
    ) -> Result<Result<OpEffect, RevertReason>, ExecError> {
        match op {
            Op::Create {
                key,
                expires_at,
                creation_flags,
                attributes,
            } => {
                // A reserved key is taken by its record (an account's, the fee
                // schedule's), though it holds no entity. A derived key lands
                // there only by a hash collision.
                if entity_records::is_reserved_key(*key)
                    || self.current(state, meter, *key)?.is_some()
                {
                    return Ok(Err(RevertReason::AlreadyExists { key: *key }));
                }
                if let Err(reason) = admit_lifetime(env, *expires_at) {
                    return Ok(Err(reason));
                }
                let entity = Entity {
                    key: *key,
                    creator: env.caller,
                    owner: env.caller,
                    created_at: env.block_number,
                    updated_at: env.block_number,
                    expires_at: *expires_at,
                    creation_flags: *creation_flags,
                    attributes: attributes.clone(),
                };
                let details = meter.count_write(
                    state
                        .update_entity(EntityUpdates::create(entity))
                        .map_err(ExecError::store)?,
                );
                // A create buys its whole size, which is all the write added,
                // until it expires.
                meter.buy(
                    self.growth(&details).and_then(|units| {
                        storage_time::create(units, *expires_at, env.block_number)
                    }),
                );
                // The create's own minting-nonce step. The decoder derived this
                // key from the nonce it read at the start of the transaction,
                // from this same view, so the creates advance it in that order.
                meter.count(
                    state
                        .fetch_increment_entity_creation_nonce(env.caller)
                        .map_err(ExecError::store)?,
                );
                Ok(Ok(OpEffect {
                    key: *key,
                    kind: OpKind::Create,
                    owner: env.caller,
                    expires_at: *expires_at,
                    creation_flags: *creation_flags,
                    previous_owner: None,
                }))
            }
            Op::Patch { key, mutations } => {
                let mutated = match self.mutate(
                    env,
                    state,
                    meter,
                    *key,
                    Auth::OwnerOnly,
                    // `readonly` freezes contents; lifecycle ops still work.
                    |e| {
                        if e.creation_flags.is_readonly() {
                            return Err(RevertReason::ReadOnly { key: *key });
                        }
                        Ok(())
                    },
                    |e| {
                        apply_mutations(e, mutations);
                        if e.attributes.len() > ENTITY_MAX_ATTRIBUTES {
                            return Err(RevertReason::TooManyAttributes {
                                count: e.attributes.len(),
                                max: ENTITY_MAX_ATTRIBUTES,
                            });
                        }
                        Ok(())
                    },
                )? {
                    Ok(mutated) => mutated,
                    Err(reason) => return Ok(Err(reason)),
                };
                // A patch buys its net growth, the write's own change, until
                // the entity expires; the size before cancels out.
                meter.buy(self.growth(&mutated.details).and_then(|units| {
                    storage_time::patch(0, units, mutated.expires_at, env.block_number)
                }));
                Ok(Ok(self.effect(
                    *key,
                    OpKind::Patch,
                    mutated.owner,
                    mutated.expires_at,
                )))
            }
            Op::ExtendExpiry {
                key,
                new_expires_at,
            } => {
                let mutated = match self.mutate(
                    env,
                    state,
                    meter,
                    *key,
                    // Anyone may pay to keep an entity alive when the flag says
                    // so — extending can't hurt the owner.
                    Auth::OwnerOrFlag(CreationFlags::PERMISSIONLESS_EXTENSION),
                    // Lifetimes never shorten; equal is a satisfied no-op, so
                    // only an extend that moves the expiry meets the bound.
                    |e| {
                        if *new_expires_at < e.expires_at {
                            return Err(RevertReason::ExpiryNotExtended {
                                key: *key,
                                new_expires_at: *new_expires_at,
                                current_expires_at: e.expires_at,
                            });
                        }
                        if *new_expires_at > e.expires_at {
                            admit_lifetime(env, *new_expires_at)?;
                        }
                        Ok(())
                    },
                    |e| {
                        e.expires_at = *new_expires_at;
                        Ok(())
                    },
                )? {
                    Ok(mutated) => mutated,
                    Err(reason) => return Ok(Err(reason)),
                };
                // An extend buys the size the entity has, from its read, for
                // the blocks it adds. Its own write changes no size: the
                // expiry is a fixed-width cell.
                meter.buy(
                    units::storage_units(&mutated.meta_before, &self.schedule).and_then(|units| {
                        storage_time::extend(units, mutated.expires_before, mutated.expires_at)
                    }),
                );
                Ok(Ok(self.effect(
                    *key,
                    OpKind::ExtendExpiry,
                    mutated.owner,
                    mutated.expires_at,
                )))
            }
            Op::Transfer { key, new_owner } => {
                let previous_owner = self.current(state, meter, *key)?.map(|e| e.entity.owner);
                self.mutate(
                    env,
                    state,
                    meter,
                    *key,
                    Auth::OwnerOnly,
                    |e| {
                        if *new_owner == e.owner {
                            return Err(RevertReason::TransferToSelf { key: *key });
                        }
                        Ok(())
                    },
                    |e| {
                        e.owner = *new_owner;
                        Ok(())
                    },
                )
                .map(|r| {
                    r.map(|mutated| OpEffect {
                        previous_owner,
                        ..self.effect(*key, OpKind::Transfer, mutated.owner, mutated.expires_at)
                    })
                })
            }
            Op::Delete { key } => {
                let Some(StoredEntity { entity, .. }) = self.current(state, meter, *key)? else {
                    return Ok(Err(RevertReason::NotFound { key: *key }));
                };
                if entity.owner != env.caller {
                    return Ok(Err(RevertReason::NotOwner {
                        key: *key,
                        caller: env.caller,
                        owner: entity.owner,
                    }));
                }
                meter.count(
                    state
                        .update_entity(EntityUpdates::deletion(*key))
                        .map_err(ExecError::store)?,
                );
                Ok(Ok(self.effect(
                    *key,
                    OpKind::Delete,
                    entity.owner,
                    entity.expires_at,
                )))
            }
        }
    }

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

    /// The storage units a write added: its net growth, 0 when it shrank the
    /// record or kept its size.
    fn growth(&self, details: &WriteDetails) -> Result<u64, Overflow> {
        let delta = units::units_delta(details, &self.schedule)?;
        u64::try_from(delta.max(0)).map_err(|_| Overflow)
    }

    /// Load, authorize, check liveness, run the op's own check, edit, and
    /// write the fields that changed. An expired entity is not mutable —
    /// including by extend.
    #[allow(clippy::too_many_arguments)]
    fn mutate(
        &self,
        env: &ExecEnv,
        state: &mut V,
        meter: &mut BatchMeter,
        key: EntityAddress,
        auth: Auth,
        check: impl FnOnce(&Entity) -> Result<(), RevertReason>,
        edit: impl FnOnce(&mut Entity) -> Result<(), RevertReason>,
    ) -> Result<Result<Mutated, RevertReason>, ExecError> {
        let Some(StoredEntity {
            entity: before,
            meta: meta_before,
        }) = self.current(state, meter, key)?
        else {
            return Ok(Err(RevertReason::NotFound { key }));
        };
        if !auth.permits(&before, &env.caller) {
            return Ok(Err(RevertReason::NotOwner {
                key,
                caller: env.caller,
                owner: before.owner,
            }));
        }
        if before.expires_at <= env.block_number {
            return Ok(Err(RevertReason::Expired {
                key,
                expires_at: before.expires_at,
            }));
        }
        if let Err(reason) = check(&before) {
            return Ok(Err(reason));
        }
        let mut entity = before.clone();
        if let Err(reason) = edit(&mut entity) {
            return Ok(Err(reason));
        }
        entity.updated_at = env.block_number;
        let details = match updates_between(key, &before, &entity) {
            Some(updates) => {
                meter.count_write(state.update_entity(updates).map_err(ExecError::store)?)
            }
            None => WriteDetails::ZERO,
        };
        Ok(Ok(Mutated {
            owner: entity.owner,
            expires_at: entity.expires_at,
            meta_before,
            expires_before: before.expires_at,
            details,
        }))
    }

    /// The entity as this transaction sees it, with its record's size: through
    /// the branch, so with every earlier op's writes.
    fn current(
        &self,
        state: &V,
        meter: &mut BatchMeter,
        key: EntityAddress,
    ) -> Result<Option<StoredEntity>, ExecError> {
        state
            .get_entity_with_meta(key, ReadMode::ViewWithOverlay)
            .map(|read| meter.count(read))
            .map_err(ExecError::store)
    }
}

/// What a mutation found and did: the entity's owner and expiry after it, the
/// record's size and expiry before it, and what its write did to the record.
#[derive(Debug)]
struct Mutated {
    owner: UserAddress,
    expires_at: BlockNumber,
    meta_before: RecordMeta,
    expires_before: BlockNumber,
    /// Zero when nothing changed, so nothing was written.
    details: WriteDetails,
}

/// One op's change to an existing entity: only the fields that differ. `None`
/// when nothing changed, so an op that changes nothing writes nothing.
fn updates_between(key: EntityAddress, before: &Entity, after: &Entity) -> Option<EntityUpdates> {
    let (b, a) = (before, after);
    let updates = EntityUpdates {
        entity: key,
        delete: false,
        creator: (b.creator != a.creator).then_some(a.creator),
        owner: (b.owner != a.owner).then_some(a.owner),
        created_at: (b.created_at != a.created_at).then_some(a.created_at),
        updated_at: (b.updated_at != a.updated_at).then_some(a.updated_at),
        expires_at: (b.expires_at != a.expires_at).then_some(a.expires_at),
        creation_flags: (b.creation_flags != a.creation_flags).then_some(a.creation_flags),
        attributes: (b.attributes != a.attributes).then(|| a.attributes.clone()),
    };
    (updates
        != EntityUpdates {
            entity: key,
            ..EntityUpdates::default()
        })
    .then_some(updates)
}

/// Who may perform a mutating op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    OwnerOnly,
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

/// A batch's costs as its ops run: the meter, and whether a receipt's cost no
/// longer fit it. A sum that overflows is out of gas, never an error, so the
/// op that hit it finishes and the batch then reverts.
#[derive(Debug, Default)]
struct BatchMeter {
    gas: GasMeter,
    overflowed: bool,
}

impl BatchMeter {
    /// A meter that already holds the GolemDB work done before the batch.
    fn starting_at(golemdb_cost: Gas) -> Self {
        let mut meter = Self::default();
        meter.count(Metered::new(
            (),
            Receipt {
                cost: CostUnits(golemdb_cost),
                ..Receipt::default()
            },
        ));
        meter
    }

    /// Count a store call's receipt and hand back its value.
    fn count<T>(&mut self, metered: Metered<T>) -> T {
        if self.gas.add_golemdb(&metered.receipt).is_err() {
            self.overflowed = true;
        }
        metered.value
    }

    /// Count a write's receipt and hand back what it did to the record.
    fn count_write(&mut self, metered: Metered<()>) -> WriteDetails {
        let details = metered.receipt.details;
        self.count(metered);
        details
    }

    /// Add the storage time an op bought; time that does not fit is out of
    /// gas, like a cost.
    fn buy(&mut self, unit_blocks: Result<u128, Overflow>) {
        if unit_blocks
            .and_then(|time| self.gas.add_storage_time(time))
            .is_err()
        {
            self.overflowed = true;
        }
    }
}

/// Running out of gas consumes all of it, and buys no storage.
fn out_of_gas(env: &ExecEnv) -> ExecOutput {
    ExecOutput {
        status: ExecStatus::Reverted,
        gas: GasBreakdown {
            compute_gas: env.gas_supplied,
            storage_gas: 0,
            purge_gas: 0,
        },
        revert: Some(RevertReason::OutOfGas),
    }
}

/// A create or extend may not leave more than the fee schedule's
/// `max_lifetime`, nor ask for permanent expiry. A business rule: the batch
/// reverts and pays for the operations it ran.
fn admit_lifetime(env: &ExecEnv, expires_at: BlockNumber) -> Result<(), RevertReason> {
    if storage_time::within_max_lifetime(expires_at, env.block_number, env.max_lifetime) {
        return Ok(());
    }
    Err(RevertReason::ExpiryBeyondMaxLifetime {
        target: expires_at,
        current_block: env.block_number,
        max_lifetime: env.max_lifetime,
    })
}

/// Set some attributes, unset others, leave the rest untouched. Every `$` name
/// was rejected at decode.
fn apply_mutations(entity: &mut Entity, mutations: &[AttributeMutation]) {
    for m in mutations {
        let name = m.key.as_slice();
        match &m.value {
            Some(value) => {
                let set = Attribute {
                    key: name.to_vec(),
                    value: value.clone(),
                    indexed: m.indexed,
                };
                match entity.attributes.iter_mut().find(|a| a.key == name) {
                    // A set replaces the attribute whole: value, type and indexed flag.
                    Some(existing) => *existing = set,
                    None => entity.attributes.push(set),
                }
            }
            // Unsetting an unset attribute is a no-op, not an error.
            None => entity.attributes.retain(|a| a.key != name),
        }
    }
    // The stored order is canonical, and a push above may have broken it.
    entity.attributes.sort_by(|a, b| a.key.cmp(&b.key));
}

/// Errors that abort execution as a host/store fault (as opposed to a revert,
/// which is a normal [`ExecOutput`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    Decode(String),
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
    use arkiv_golemdb_state::GolemStateView;
    use arkiv_interfaces::constants::OP_MAX_ATTRIBUTES;
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::primitives::UserBalance;
    use arkiv_interfaces::statemanager::{
        AccountBalancesStore, BlockRef, EntityCreationNoncesStore, EntityStore,
    };

    use arkiv_interfaces::store::priced::{Priced, Prices};
    use arkiv_interfaces::store::reference::MemStore;
    use arkiv_interfaces::store::{RecordKey, Store, StoreExt};
    use std::sync::Arc;

    type View = GolemStateView<Arc<MemStore>>;

    const GENESIS: BlockRef = BlockRef {
        height: 9,
        hash: [0xBB; 32],
    };

    /// The op executor's state view is a GolemDB store: these tests exercise op
    /// semantics, so they run against the same backend the node does.
    fn fresh_view() -> View {
        seeded_view(&[])
    }

    /// A view whose base commit already holds `entities`.
    fn seeded_view(entities: &[Entity]) -> View {
        let store = Arc::new(MemStore::new());
        let branch = store.begin(None).expect("begin");
        {
            let mut staging = GolemStateView::new(
                store.clone(),
                branch,
                store.head(),
                GENESIS,
                arkiv_interfaces::statemanager::SessionId([0; 16]),
            );
            for entity in entities {
                staging
                    .update_entity(EntityUpdates::create(entity.clone()))
                    .unwrap();
            }
        }
        let base = store
            .commit_tagged(branch, GENESIS.hash)
            .expect("commit base");
        let branch = store.begin(Some(base)).expect("begin");
        GolemStateView::new(
            store,
            branch,
            base,
            GENESIS,
            arkiv_interfaces::statemanager::SessionId([1; 16]),
        )
    }

    type PricedView = GolemStateView<Priced<MemStore>>;

    /// An empty view over a store that prices its calls: a read 1, a create
    /// 10, a patch 100, a delete 1,000.
    fn priced_view(store: Priced<MemStore>) -> PricedView {
        let origin = store.head();
        let branch = store.begin(Some(origin)).expect("begin");
        GolemStateView::new(
            store,
            branch,
            origin,
            GENESIS,
            arkiv_interfaces::statemanager::SessionId([2; 16]),
        )
    }

    /// A create on an empty view spends 23 in GolemDB's units: its existence
    /// read (1); the write, which reads again and creates (1 + 10); and the
    /// minting nonce, read and created for a new account (1 + 10).
    const CREATE_ON_EMPTY: u64 = 23;

    /// An environment that charges the transaction's own terms and a floor.
    fn priced_env(caller: [u8; 20], tx_gas: u64, floor_gas: u64) -> ExecEnv {
        ExecEnv {
            tx_gas,
            floor_gas,
            ..env(caller, 10)
        }
    }

    /// Compute is the transaction's terms, each op's own cost and every store
    /// call's cost, added together; the floor lifts it only when it is lower.
    /// The create's 40 blocks of storage, 0.58 gas for the empty record's
    /// [`EMPTY_RECORD_UNITS`], round up to 1.
    #[test]
    fn compute_adds_the_terms_the_ops_and_golemdbs_work() {
        let exec = ArkivExecutor::<PricedView>::new();
        let create = create_until(1, 50);
        let op = PlaceholderCost.op_cost(&create);

        let mut view = priced_view(Priced::new(MemStore::new(), Prices::DISTINCT));
        let out = exec
            .apply(
                &priced_env([0xAA; 20], 21_000, 0),
                &mut view,
                core::slice::from_ref(&create),
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(
            out.gas,
            GasBreakdown {
                compute_gas: 21_000 + op + CREATE_ON_EMPTY,
                storage_gas: 1,
                purge_gas: 0,
            }
        );

        let mut view = priced_view(Priced::new(MemStore::new(), Prices::DISTINCT));
        let floor = 21_000 + op + CREATE_ON_EMPTY + 1;
        let out = exec
            .apply(&priced_env([0xAA; 20], 21_000, floor), &mut view, &[create])
            .unwrap();
        assert_eq!(out.gas.compute_gas, floor, "a higher floor lifts it, once");
    }

    /// An entity record without attributes, in storage units at the
    /// placeholder weights: 1 per byte, 64 per cell and per indexed cell. A
    /// cell takes (8 + name) + (1 + value) bytes, an index entry
    /// name + 2 + value:
    ///
    /// ```text
    /// cell             cell bytes   index bytes
    /// $key             12 + 33      38
    /// $creator         16 + 21      30
    /// $owner           14 + 21      28
    /// $createdAt       18 + 9       -
    /// $expiresAt       18 + 9       20
    /// $updatedAt       18 + 9       -
    /// $creationFlags   22 + 2       -
    /// ```
    ///
    /// 222 cell bytes, 116 index bytes, 7 cells, 4 of them indexed:
    /// 222 + 116 + 7 × 64 + 4 × 64 = 1,042.
    const EMPTY_RECORD_UNITS: u64 = 1_042;

    /// Long enough that storage gas reads well: 100,000 blocks.
    const LIFETIME: u64 = 100_000;

    /// An unindexed `note` of 1,000 bytes, in storage units: its cell,
    /// (8 + 4) + (1 + 1,000) bytes, and 64 for the cell.
    const NOTE_UNITS: u64 = 1_077;

    fn note() -> AttributeMutation {
        AttributeMutation::set_unindexed(b"note".to_vec(), AttributeValue::Str("x".repeat(1_000)))
    }

    /// An entity `owner` holds without attributes, alive until `expires_at`.
    fn owned_until(key: u8, owner: [u8; 20], expires_at: u64) -> Entity {
        Entity {
            key: [key; 32],
            creator: owner,
            owner,
            created_at: 3,
            updated_at: 3,
            expires_at,
            creation_flags: CreationFlags::NONE,
            attributes: Vec::new(),
        }
    }

    /// A create pays compute and buys storage: its whole record,
    /// [`EMPTY_RECORD_UNITS`], for the [`LIFETIME`] until it expires.
    /// 104,200,000 unit-blocks at 14 micro-gas each are 1,458.8 gas, rounded
    /// up. No purge is charged.
    #[test]
    fn a_create_buys_its_size_until_expiry() {
        let exec = ArkivExecutor::<PricedView>::new();
        let create = create_until(1, 10 + LIFETIME);
        let op = PlaceholderCost.op_cost(&create);

        let mut view = priced_view(Priced::new(MemStore::new(), Prices::DISTINCT));
        let out = exec
            .apply(
                &priced_env([0xAA; 20], 21_000, 0),
                &mut view,
                core::slice::from_ref(&create),
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(
            out.gas,
            GasBreakdown {
                compute_gas: 21_000 + op + CREATE_ON_EMPTY,
                storage_gas: 1_459,
                purge_gas: 0,
            }
        );
        assert_eq!(out.gas_used(), 21_000 + op + CREATE_ON_EMPTY + 1_459);
    }

    /// A patch buys its growth only, for the rest of the entity's life:
    /// [`NOTE_UNITS`] for 100,000 blocks, 1,507.8 gas, rounded up. Removing the
    /// note again buys nothing and refunds nothing.
    #[test]
    fn a_patch_buys_only_its_growth() {
        let exec = ArkivExecutor::<View>::new();
        let alice = [0xAA; 20];
        let mut view = seeded_view(&[owned_until(1, alice, 10 + LIFETIME)]);
        let grow = Op::Patch {
            key: [1; 32],
            mutations: vec![note()],
        };
        let out = exec
            .apply(&env(alice, 10), &mut view, core::slice::from_ref(&grow))
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 1_508);

        let shrink = Op::Patch {
            key: [1; 32],
            mutations: vec![AttributeMutation::unset(b"note".to_vec())],
        };
        let out = exec.apply(&env(alice, 10), &mut view, &[shrink]).unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 0, "a shrink");
    }

    /// An extend buys the size the entity has for the blocks it adds:
    /// [`EMPTY_RECORD_UNITS`] for 100,000 more blocks, 1,459 gas. An extend to
    /// the same expiry buys nothing. The size is the branch's: after a growing
    /// patch in the same batch, the extend buys the grown record,
    /// (1,042 + 1,077) × 100,000, on top of the patch's 1,077 × 100,000:
    /// 319,600,000 unit-blocks, 4,474.4 gas, rounded up once.
    #[test]
    fn an_extend_buys_only_the_added_blocks() {
        let exec = ArkivExecutor::<View>::new();
        let alice = [0xAA; 20];
        let expires_at = 10 + LIFETIME;
        let extend = |new_expires_at| Op::ExtendExpiry {
            key: [1; 32],
            new_expires_at,
        };

        let mut view = seeded_view(&[owned_until(1, alice, expires_at)]);
        let out = exec
            .apply(&env(alice, 10), &mut view, &[extend(expires_at + LIFETIME)])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 1_459);

        let mut view = seeded_view(&[owned_until(1, alice, expires_at)]);
        let out = exec
            .apply(&env(alice, 10), &mut view, &[extend(expires_at)])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 0, "no blocks added");

        let mut view = seeded_view(&[owned_until(1, alice, expires_at)]);
        let grow = Op::Patch {
            key: [1; 32],
            mutations: vec![note()],
        };
        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[grow, extend(expires_at + LIFETIME)],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(
            u128::from(NOTE_UNITS + (EMPTY_RECORD_UNITS + NOTE_UNITS)) * u128::from(LIFETIME),
            319_600_000
        );
        assert_eq!(out.gas.storage_gas, 4_475);
    }

    /// A transfer and a delete buy no storage.
    #[test]
    fn a_transfer_and_a_delete_buy_nothing() {
        let exec = ArkivExecutor::<View>::new();
        let alice = [0xAA; 20];
        let mut view = seeded_view(&[
            owned_until(1, alice, 10 + LIFETIME),
            owned_until(2, alice, 10 + LIFETIME),
        ]);
        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[
                    Op::Transfer {
                        key: [1; 32],
                        new_owner: [0xBB; 20],
                    },
                    Op::Delete { key: [2; 32] },
                ],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 0);
    }

    /// Storage gas is rounded once per batch, not per op: two creates of 20
    /// blocks buy 20,840 unit-blocks each, 0.29 gas, so 1 together rather than
    /// 1 each.
    #[test]
    fn storage_gas_is_rounded_once_per_batch() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let out = exec
            .apply(
                &env([0xAA; 20], 10),
                &mut view,
                &[create_until(1, 30), create_until(2, 30)],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 1);
    }

    /// Storage counts against the supplied gas: a create whose compute fits
    /// but whose 1,459 storage gas does not runs out of gas, pays all it
    /// supplied, and leaves nothing behind. One gas more and it succeeds.
    #[test]
    fn storage_can_run_a_batch_out_of_gas() {
        let exec = ArkivExecutor::<View>::new();
        let alice = [0xAA; 20];
        let create = create_until(1, 10 + LIFETIME);
        let needed = PlaceholderCost.op_cost(&create) + 1_459;

        let mut view = fresh_view();
        let out = exec
            .apply(
                &env_gas(alice, 10, needed - 1),
                &mut view,
                core::slice::from_ref(&create),
            )
            .unwrap();
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert_eq!(out.gas_used(), needed - 1);
        assert_eq!(out.gas.storage_gas, 0);
        assert!(staged(&view, [1; 32]).is_none());
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
        assert_eq!(view.open_frames(), 0);

        let mut view = fresh_view();
        let out = exec
            .apply(&env_gas(alice, 10, needed), &mut view, &[create])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas_used(), needed);
    }

    /// A patch that removes as well as adds buys its net growth. Swapping the
    /// note for a `memo` of the same size buys nothing; swapping that for a
    /// 2,000-byte note grows the record by 1,000 units, 1,400 gas for 100,000
    /// blocks.
    #[test]
    fn a_patch_that_grows_and_shrinks_buys_its_net_growth() {
        let exec = ArkivExecutor::<View>::new();
        let alice = [0xAA; 20];
        let mut view = seeded_view(&[owned_until(1, alice, 10 + LIFETIME)]);
        let patch = |mutations| Op::Patch {
            key: [1; 32],
            mutations,
        };
        let text = |len| AttributeValue::Str("x".repeat(len));
        let mut run = |op| {
            let out = exec.apply(&env(alice, 10), &mut view, &[op]).unwrap();
            assert_eq!(out.status, ExecStatus::Ok);
            out.gas.storage_gas
        };

        assert_eq!(run(patch(vec![note()])), 1_508);
        assert_eq!(
            run(patch(vec![
                AttributeMutation::unset(b"note".to_vec()),
                AttributeMutation::set_unindexed(b"memo".to_vec(), text(1_000)),
            ])),
            0,
            "an equal swap"
        );
        assert_eq!(
            run(patch(vec![
                AttributeMutation::unset(b"memo".to_vec()),
                AttributeMutation::set_unindexed(b"note".to_vec(), text(2_000)),
            ])),
            1_400
        );
    }

    /// A create with attributes buys them too, an indexed one with its index
    /// entry. `tag = "blue"`, indexed: its cell, (8 + 3) + (1 + 4) bytes, 64
    /// for the cell, its index entry, 3 + 2 + 4 bytes, and 64 for that, 153
    /// units. With the note and the record itself, 1,042 + 1,077 + 153 =
    /// 2,272 units for 100,000 blocks: 3,180.8 gas, rounded up.
    #[test]
    fn a_create_buys_its_attributes_and_their_index_entries() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let create = Op::Create {
            key: [1; 32],
            expires_at: 10 + LIFETIME,
            creation_flags: CreationFlags::NONE,
            attributes: vec![
                Attribute::unindexed(b"note".to_vec(), AttributeValue::Str("x".repeat(1_000))),
                Attribute::new(b"tag".to_vec(), AttributeValue::Str("blue".into())),
            ],
        };
        let out = exec
            .apply(&env([0xAA; 20], 10), &mut view, &[create])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas.storage_gas, 3_181);
    }

    /// Storage gas that does not fit `u64` is out of gas, like a cost: at a
    /// `capacity_rate` of `u64::MAX`, the empty create's storage is about
    /// 1.9 × 10²¹ gas. All the gas supplied is charged, none of it as storage,
    /// and the create is undone.
    #[test]
    fn storage_gas_that_does_not_fit_is_out_of_gas() {
        let exec = ArkivExecutor::<View>::new().with_schedule(FeeSchedule {
            capacity_rate: u64::MAX,
            ..DEFAULT_SCHEDULE
        });
        let mut view = fresh_view();
        let out = exec
            .apply(
                &env([0xAA; 20], 10),
                &mut view,
                &[create_until(1, 10 + LIFETIME)],
            )
            .unwrap();
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert_eq!(
            out.gas,
            GasBreakdown {
                compute_gas: AMPLE_GAS,
                storage_gas: 0,
                purge_gas: 0,
            }
        );
        assert!(staged(&view, [1; 32]).is_none());
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
        assert_eq!(view.open_frames(), 0);
    }

    /// A business-rule revert pays for the work it did, GolemDB's on writes
    /// now undone included, and buys no storage: the second create's existence
    /// read (1) adds to the first create's 23.
    #[test]
    fn a_revert_pays_for_the_golemdb_work_it_undid() {
        let exec = ArkivExecutor::<PricedView>::new();
        let ops = [create_until(1, 50), create_until(1, 50)];
        let op = PlaceholderCost.op_cost(&ops[0]);

        let mut view = priced_view(Priced::new(MemStore::new(), Prices::DISTINCT));
        let out = exec
            .apply(&priced_env([0xAA; 20], 21_000, 0), &mut view, &ops)
            .unwrap();
        assert_eq!(
            out.revert,
            Some(RevertReason::AlreadyExists { key: [1; 32] })
        );
        assert_eq!(
            out.gas,
            GasBreakdown {
                compute_gas: 21_000 + 2 * op + CREATE_ON_EMPTY + 1,
                storage_gas: 0,
                purge_gas: 0,
            }
        );
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
    }

    /// GolemDB's work counts against the supplied gas too: a batch whose op
    /// costs fit but whose store calls do not runs out of gas, paying it all.
    #[test]
    fn golemdbs_work_can_run_a_batch_out_of_gas() {
        let exec = ArkivExecutor::<PricedView>::new();
        let create = create_until(1, 50);
        let op = PlaceholderCost.op_cost(&create);
        let supplied = op + CREATE_ON_EMPTY - 1;

        let mut view = priced_view(Priced::new(MemStore::new(), Prices::DISTINCT));
        let out = exec
            .apply(&env_gas([0xAA; 20], 10, supplied), &mut view, &[create])
            .unwrap();
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert_eq!(out.gas_used(), supplied);
        assert!(staged_in(&view, [1u8; 32]).is_none());
    }

    /// Receipt costs whose sum does not fit the meter are out of gas, never an
    /// error or a saturated charge. Each call fits on its own: the existence
    /// read and the write's own read together pass `u64::MAX`.
    #[test]
    fn a_cost_that_does_not_fit_runs_out_of_gas() {
        let exec = ArkivExecutor::<PricedView>::new();
        let prices = Prices {
            get: u64::MAX / 2 + 1,
            ..Prices::DISTINCT
        };
        let mut view = priced_view(Priced::new(MemStore::new(), prices));
        let out = exec
            .apply(&env([0xAA; 20], 10), &mut view, &[create_until(1, 50)])
            .unwrap();
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert_eq!(out.gas_used(), AMPLE_GAS);
        assert_eq!(view.open_frames(), 0);
    }

    /// A store fault in mid-batch is an error, not a revert, and the frame is
    /// closed before it is returned: the first create's writes are undone and
    /// the view takes another batch. Writes 0 and 1 are the first create and
    /// its new account; write 2, the second create, fails.
    #[test]
    fn a_store_fault_mid_batch_closes_the_frame() {
        let exec = ArkivExecutor::<PricedView>::new();
        let alice = [0xAA; 20];
        let mut view = priced_view(Priced::new(MemStore::new(), Prices::DISTINCT).failing_write(2));

        let mut effects = Vec::new();
        let fault = exec.apply_with_effects(
            &env(alice, 10),
            &mut view,
            &[create_until(1, 50), create_until(2, 50)],
            &mut effects,
        );
        assert!(fault.is_err(), "a store fault is an error: {fault:?}");
        assert_eq!(view.open_frames(), 0);
        assert!(staged_in(&view, [1u8; 32]).is_none());
        assert!(effects.is_empty());
        assert_eq!(
            view.get_entity_creation_nonce(alice, ReadMode::ViewWithOverlay)
                .unwrap()
                .value
                .get(),
            0
        );

        let out = exec
            .apply(&env(alice, 10), &mut view, &[create_until(3, 50)])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok, "the view takes another batch");
    }

    fn staged_in<S: Store>(view: &GolemStateView<S>, key: EntityAddress) -> Option<Entity> {
        view.get_entity(key, ReadMode::ViewWithOverlay)
            .unwrap()
            .value
    }

    fn staged(view: &View, key: EntityAddress) -> Option<Entity> {
        view.get_entity(key, ReadMode::ViewWithOverlay)
            .unwrap()
            .value
    }

    /// Comfortably more gas than any op here costs.
    const AMPLE_GAS: u64 = 100_000_000;

    fn env(caller: [u8; 20], block: u64) -> ExecEnv {
        env_gas(caller, block, AMPLE_GAS)
    }

    /// The genesis files' `max_lifetime`: six months of 2-second blocks.
    const MAX_LIFETIME: u64 = 7_884_000;

    fn env_gas(caller: [u8; 20], block: u64, gas_supplied: u64) -> ExecEnv {
        ExecEnv {
            caller,
            block_number: block,
            gas_supplied,
            tx_gas: 0,
            floor_gas: 0,
            golemdb_cost_before: 0,
            chain_id: 1,
            max_lifetime: MAX_LIFETIME,
            keep_writes: true,
        }
    }

    fn sample_entity() -> Entity {
        Entity {
            key: [7u8; 32],
            creator: [1u8; 20],
            owner: [2u8; 20],
            created_at: 3,
            updated_at: 4,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            attributes: vec![
                Attribute::new(b"color".to_vec(), AttributeValue::Str("blue".into())),
                Attribute::new(b"size".to_vec(), AttributeValue::u256_from_u64(1)),
            ],
        }
    }

    /// A create stages the entity into the view with env-resolved lifecycle
    /// fields: the caller is creator and owner, the block stamps creation.
    #[test]
    fn create_stages_into_the_view() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let alice = [0xAA; 20];

        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[Op::Create {
                    key: [1u8; 32],
                    expires_at: 50,
                    creation_flags: CreationFlags::NONE,
                    attributes: Vec::new(),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        let e = staged(&view, [1u8; 32]).expect("staged in the view overlay");
        assert_eq!(e.owner, alice);
        assert_eq!(e.creator, alice);
        assert_eq!(e.created_at, 10);
        // Not on the base until commit.
        assert!(
            view.get_entity([1u8; 32], ReadMode::ViewOnBase)
                .unwrap()
                .value
                .is_none()
        );
    }

    fn create_until(key: u8, expires_at: u64) -> Op {
        Op::Create {
            key: [key; 32],
            expires_at,
            creation_flags: CreationFlags::NONE,
            attributes: Vec::new(),
        }
    }

    /// A create may leave exactly `max_lifetime`; one block more reverts the
    /// whole batch, as does permanent expiry, which is not sold.
    #[test]
    fn a_create_is_bounded_by_max_lifetime() {
        let exec = ArkivExecutor::<View>::new();
        let alice = [0xAA; 20];
        let beyond = |target| RevertReason::ExpiryBeyondMaxLifetime {
            target,
            current_block: 10,
            max_lifetime: MAX_LIFETIME,
        };

        let mut view = fresh_view();
        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[create_until(1, 10 + MAX_LIFETIME)],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok, "exactly the bound");

        let mut view = fresh_view();
        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[
                    create_until(1, 10 + MAX_LIFETIME),
                    create_until(2, 10 + MAX_LIFETIME + 1),
                ],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Reverted);
        assert_eq!(out.revert, Some(beyond(10 + MAX_LIFETIME + 1)));
        assert!(
            view.get_uncommitted_deltas().unwrap().is_empty(),
            "the create before it is reverted with it"
        );

        let mut view = fresh_view();
        let out = exec
            .apply(&env(alice, 10), &mut view, &[create_until(1, u64::MAX)])
            .unwrap();
        assert_eq!(out.revert, Some(beyond(u64::MAX)), "permanent expiry");
    }

    /// The bound is on the lifetime an extend leaves, not on the blocks it
    /// adds: one block more than `max_lifetime` in total reverts, although a
    /// single added block would fit on its own.
    #[test]
    fn an_extend_is_bounded_by_what_remains() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]); // expires_at 100
        let owner = [2u8; 20];
        let extend = |new_expires_at| Op::ExtendExpiry {
            key: [7u8; 32],
            new_expires_at,
        };
        let beyond = |target| RevertReason::ExpiryBeyondMaxLifetime {
            target,
            current_block: 20,
            max_lifetime: MAX_LIFETIME,
        };

        let out = exec
            .apply(&env(owner, 20), &mut view, &[extend(20 + MAX_LIFETIME)])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok, "exactly the bound");

        let out = exec
            .apply(&env(owner, 20), &mut view, &[extend(20 + MAX_LIFETIME + 1)])
            .unwrap();
        assert_eq!(out.revert, Some(beyond(20 + MAX_LIFETIME + 1)));

        let out = exec
            .apply(&env(owner, 20), &mut view, &[extend(u64::MAX)])
            .unwrap();
        assert_eq!(out.revert, Some(beyond(u64::MAX)), "permanent expiry");
        assert_eq!(
            staged(&view, [7u8; 32]).unwrap().expires_at,
            20 + MAX_LIFETIME
        );
    }

    /// An extend that does not move the expiry stays a no-op, even for an
    /// entity that already lives longer than today's bound allows.
    #[test]
    fn a_no_op_extend_is_not_bounded() {
        let exec = ArkivExecutor::<View>::new();
        let long_lived = Entity {
            expires_at: 20 + MAX_LIFETIME + 1_000,
            ..sample_entity()
        };
        let mut view = seeded_view(&[long_lived]);
        let out = exec
            .apply(
                &env([2u8; 20], 20),
                &mut view,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 20 + MAX_LIFETIME + 1_000,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
    }

    /// A revert is all-or-nothing: nothing reaches the view.
    #[test]
    fn create_on_existing_reverts_and_stages_nothing() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[Entity {
            key: [1u8; 32],
            ..sample_entity()
        }]);

        let out = exec
            .apply(
                &env([0xAA; 20], 10),
                &mut view,
                &[Op::Create {
                    key: [1u8; 32],
                    expires_at: 50,
                    creation_flags: CreationFlags::NONE,
                    attributes: Vec::new(),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Reverted);
        assert!(out.revert.is_some());
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
    }

    /// An op can name an account's key, whose record is not an entity's. It must
    /// revert: a fault fails the whole block built around it.
    #[test]
    fn an_op_on_an_account_key_reverts() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let alice = [0xAA; 20];
        view.set_balance(alice, UserBalance::from_u64(500))
            .expect("fund");
        let key = RecordKey::from_address(alice).0;

        let not_found = RevertReason::NotFound { key };
        for (op, reason) in [
            (
                Op::Create {
                    key,
                    expires_at: 50,
                    creation_flags: CreationFlags::NONE,
                    attributes: Vec::new(),
                },
                RevertReason::AlreadyExists { key },
            ),
            (
                Op::Patch {
                    key,
                    mutations: Vec::new(),
                },
                not_found.clone(),
            ),
            (
                Op::ExtendExpiry {
                    key,
                    new_expires_at: 50,
                },
                not_found.clone(),
            ),
            (
                Op::Transfer {
                    key,
                    new_owner: [0xBB; 20],
                },
                not_found.clone(),
            ),
            (Op::Delete { key }, not_found.clone()),
        ] {
            let out = exec
                .apply(&env(alice, 10), &mut view, &[op])
                .expect("a revert, not a fault");
            assert_eq!(out.status, ExecStatus::Reverted);
            assert_eq!(out.revert, Some(reason));
        }
        assert_eq!(
            view.get_balance(alice, ReadMode::ViewWithOverlay)
                .expect("balance"),
            UserBalance::from_u64(500)
        );
    }

    /// The fee schedule's record sits beside the entities, at a key a caller can
    /// name. An op there must revert like one on an account's key: reading the
    /// schedule as an entity fails, and a fault fails the whole block.
    #[test]
    fn an_op_on_the_fee_schedule_key_reverts() {
        use arkiv_tokenomics::{FeeScheduleSource, schedule_record};

        let store = Arc::new(MemStore::new());
        let branch = store.begin(None).expect("begin");
        store
            .create(
                branch,
                schedule_record::RECORD_KEY,
                schedule_record::to_cells(&DEFAULT_SCHEDULE),
                None,
            )
            .expect("the genesis schedule");
        let base = store
            .commit_tagged(branch, GENESIS.hash)
            .expect("commit base");
        let branch = store.begin(Some(base)).expect("begin");
        let mut view = GolemStateView::new(
            store,
            branch,
            base,
            GENESIS,
            arkiv_interfaces::statemanager::SessionId([1; 16]),
        );
        let exec = ArkivExecutor::<View>::new();
        let key = entity_records::FEE_SCHEDULE_KEY;

        let not_found = RevertReason::NotFound { key };
        for (op, reason) in [
            (
                Op::Create {
                    key,
                    expires_at: 50,
                    creation_flags: CreationFlags::NONE,
                    attributes: Vec::new(),
                },
                RevertReason::AlreadyExists { key },
            ),
            (
                Op::Patch {
                    key,
                    mutations: Vec::new(),
                },
                not_found.clone(),
            ),
            (
                Op::ExtendExpiry {
                    key,
                    new_expires_at: 50,
                },
                not_found.clone(),
            ),
            (
                Op::Transfer {
                    key,
                    new_owner: [0xBB; 20],
                },
                not_found.clone(),
            ),
            (Op::Delete { key }, not_found.clone()),
        ] {
            let out = exec
                .apply(&env([0xAA; 20], 10), &mut view, &[op])
                .expect("a revert, not a fault");
            assert_eq!(out.status, ExecStatus::Reverted);
            assert_eq!(out.revert, Some(reason));
        }
        assert_eq!(view.fee_schedule(), Ok(DEFAULT_SCHEDULE), "untouched");
    }

    #[test]
    fn transfer_requires_ownership_then_moves_owner() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]);
        let owner = [2u8; 20];

        let out = exec
            .apply(
                &env([0xFF; 20], 20),
                &mut view,
                &[Op::Transfer {
                    key: [7u8; 32],
                    new_owner: [9u8; 20],
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Reverted);
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());

        let out = exec
            .apply(
                &env(owner, 20),
                &mut view,
                &[Op::Transfer {
                    key: [7u8; 32],
                    new_owner: [9u8; 20],
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        let e = staged(&view, [7u8; 32]).unwrap();
        assert_eq!(e.owner, [9u8; 20]);
        assert_eq!(e.updated_at, 20);
        // The staged delta is partial: only what the transfer changed.
        let deltas = view.get_uncommitted_deltas().unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].owner.is_some());
        assert!(deltas[0].attributes.is_none());
    }

    #[test]
    fn transfer_to_current_owner_reverts() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]);
        let owner = [2u8; 20];

        let out = exec
            .apply(
                &env(owner, 20),
                &mut view,
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
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
    }

    /// "> extends, == no-op, < reverts."
    #[test]
    fn extend_never_shortens_and_equal_is_a_no_op() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]); // expires_at 100
        let owner = [2u8; 20];

        let out = exec
            .apply(
                &env(owner, 20),
                &mut view,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 100,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(staged(&view, [7u8; 32]).unwrap().expires_at, 100);

        let out = exec
            .apply(
                &env(owner, 20),
                &mut view,
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

        let out = exec
            .apply(
                &env(owner, 20),
                &mut view,
                &[Op::ExtendExpiry {
                    key: [7u8; 32],
                    new_expires_at: 150,
                }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(staged(&view, [7u8; 32]).unwrap().expires_at, 150);
    }

    /// Ops within one transaction see each other: a delete finds the entity
    /// the create before it wrote, and leaves nothing to read. Each op writes
    /// as it runs, so the delta log keeps the deletion of an entity the base
    /// never had, which every consumer treats as nothing to remove.
    #[test]
    fn create_then_delete_in_one_tx_leaves_nothing_behind() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();

        let out = exec
            .apply(
                &env([0xAA; 20], 10),
                &mut view,
                &[
                    Op::Create {
                        key: [3u8; 32],
                        expires_at: 50,
                        creation_flags: CreationFlags::NONE,
                        attributes: Vec::new(),
                    },
                    Op::Delete { key: [3u8; 32] },
                ],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        assert!(staged(&view, [3u8; 32]).is_none());
        let deltas = view.get_uncommitted_deltas().unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].delete, "a deletion, not a live entity");
        assert_eq!(deltas[0].entity, [3u8; 32]);
    }

    fn minting_nonce(view: &View, owner: [u8; 20]) -> u64 {
        view.get_entity_creation_nonce(owner, ReadMode::ViewWithOverlay)
            .unwrap()
            .value
            .get()
    }

    /// Each create advances its creator's minting nonce as it runs; a batch
    /// that reverts advances it not at all.
    #[test]
    fn each_create_advances_the_minting_nonce() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let alice = [0xAA; 20];

        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[create_until(1, 50), create_until(2, 50)],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(minting_nonce(&view, alice), 2);

        let out = exec
            .apply(
                &env(alice, 10),
                &mut view,
                &[create_until(3, 50), create_until(1, 50)],
            )
            .unwrap();
        assert_eq!(
            out.revert,
            Some(RevertReason::AlreadyExists { key: [1; 32] })
        );
        assert_eq!(
            minting_nonce(&view, alice),
            2,
            "the reverted create's step is undone with it"
        );
    }

    /// A batch that does not keep its writes, as `eth_call` and
    /// `eth_estimateGas` run one, succeeds and leaves nothing behind: not its
    /// entities, not its minting-nonce steps.
    #[test]
    fn a_batch_that_does_not_keep_its_writes_leaves_nothing() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let alice = [0xAA; 20];
        let probe = ExecEnv {
            keep_writes: false,
            ..env(alice, 10)
        };

        let out = exec
            .apply(&probe, &mut view, &[create_until(1, 50)])
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert!(staged(&view, [1u8; 32]).is_none());
        assert_eq!(minting_nonce(&view, alice), 0);
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());

        let out = exec
            .apply(&probe, &mut view, &[create_until(1, 50)])
            .unwrap();
        assert_eq!(
            out.status,
            ExecStatus::Ok,
            "the next probe does not see the last one's entity"
        );
    }

    /// A delete stages a tombstone the index derivation turns into all-removes.
    #[test]
    fn delete_stages_a_tombstone() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]);

        let out = exec
            .apply(
                &env([2u8; 20], 20),
                &mut view,
                &[Op::Delete { key: [7u8; 32] }],
            )
            .unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        let deltas = view.get_uncommitted_deltas().unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].delete);
        assert!(staged(&view, [7u8; 32]).is_none());
    }

    /// A patch replaces contents, stamps `updated_at`, and leaves
    /// identity and lifecycle untouched — and the staged delta carries only the
    /// changed fields.
    #[test]
    fn patch_merges_content_and_stamps_modified() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]);

        let out = exec
            .apply(
                &env([2u8; 20], 20),
                &mut view,
                &[Op::Patch {
                    key: [7u8; 32],
                    mutations: vec![
                        AttributeMutation::set_unindexed(
                            b"body".to_vec(),
                            AttributeValue::Bytes(b"world".to_vec()),
                        ),
                        AttributeMutation::set_unindexed(
                            b"mimeType".to_vec(),
                            AttributeValue::Str("application/json".into()),
                        ),
                    ],
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        let e = staged(&view, [7u8; 32]).unwrap();
        assert_eq!(
            e.attributes,
            vec![
                Attribute::unindexed(b"body".to_vec(), AttributeValue::Bytes(b"world".to_vec())),
                Attribute::new(b"color".to_vec(), AttributeValue::Str("blue".into())),
                Attribute::unindexed(
                    b"mimeType".to_vec(),
                    AttributeValue::Str("application/json".into())
                ),
                Attribute::new(b"size".to_vec(), AttributeValue::u256_from_u64(1)),
            ],
            "unmentioned attributes survive, the new ones land sorted"
        );
        assert_eq!(e.updated_at, 20);
        assert_eq!(e.creator, [1u8; 20]);
        assert_eq!(e.owner, [2u8; 20]);
        assert_eq!(e.created_at, 3);
        assert_eq!(e.expires_at, 100);

        let deltas = view.get_uncommitted_deltas().unwrap();
        assert!(deltas[0].attributes.is_some());
        assert!(deltas[0].owner.is_none());
        assert!(deltas[0].expires_at.is_none());
    }

    /// A patch explicitly unsetting an attribute drops it from the staged set.
    #[test]
    fn patch_unset_drops_the_attribute() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]);

        exec.apply(
            &env([2u8; 20], 20),
            &mut view,
            &[Op::Patch {
                key: [7u8; 32],
                mutations: vec![AttributeMutation::unset(b"color".to_vec())],
            }],
        )
        .unwrap();

        let e = staged(&view, [7u8; 32]).unwrap();
        assert_eq!(e.attributes.len(), 1);
        assert_eq!(e.attributes[0].key, b"size");
    }

    /// A set replaces the attribute whole, indexed flag included — so the same
    /// value can be moved out of (and back into) the index.
    #[test]
    fn patch_can_flip_the_indexed_flag() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = seeded_view(&[sample_entity()]);
        let blue = AttributeValue::Str("blue".into());

        exec.apply(
            &env([2u8; 20], 20),
            &mut view,
            &[Op::Patch {
                key: [7u8; 32],
                mutations: vec![AttributeMutation::set_unindexed(
                    b"color".to_vec(),
                    blue.clone(),
                )],
            }],
        )
        .unwrap();
        let e = staged(&view, [7u8; 32]).unwrap();
        assert_eq!(
            e.attributes[0],
            Attribute::unindexed(b"color".to_vec(), blue)
        );
    }

    #[test]
    fn patches_cannot_exceed_the_entity_attribute_limit() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let owner = [0xAA; 20];
        let key = [8u8; 32];

        let mutations = |start: usize, count: usize| {
            (start..start + count)
                .map(|i| {
                    AttributeMutation::set(
                        format!("attribute-{i:03}").into_bytes(),
                        AttributeValue::u256_from_u64(i as u64),
                    )
                })
                .collect::<Vec<_>>()
        };

        let mut ops = vec![Op::Create {
            key,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            attributes: Vec::new(),
        }];
        for start in (0..ENTITY_MAX_ATTRIBUTES).step_by(OP_MAX_ATTRIBUTES) {
            ops.push(Op::Patch {
                key,
                mutations: mutations(start, OP_MAX_ATTRIBUTES),
            });
        }

        let out = exec.apply(&env(owner, 10), &mut view, &ops).unwrap();
        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(
            staged(&view, key).unwrap().attributes.len(),
            ENTITY_MAX_ATTRIBUTES
        );

        let out = exec
            .apply(
                &env(owner, 11),
                &mut view,
                &[Op::Patch {
                    key,
                    mutations: mutations(ENTITY_MAX_ATTRIBUTES, 1),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Reverted);
        assert_eq!(
            out.revert,
            Some(RevertReason::TooManyAttributes {
                count: ENTITY_MAX_ATTRIBUTES + 1,
                max: ENTITY_MAX_ATTRIBUTES,
            })
        );
        assert_eq!(
            staged(&view, key).unwrap().attributes.len(),
            ENTITY_MAX_ATTRIBUTES
        );
    }

    /// Expiry is final: at `expires_at` the owner can no longer mutate — you
    /// cannot resurrect by extending after the fact.
    #[test]
    fn cannot_mutate_after_expiry() {
        let exec = ArkivExecutor::<View>::new();
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
            let mut view = seeded_view(&[sample_entity()]);
            let out = exec
                .apply(&env(owner, 100), &mut view, std::slice::from_ref(&op))
                .unwrap();
            assert_eq!(
                out.status,
                ExecStatus::Reverted,
                "{:?} should revert post-expiry",
                op.kind()
            );
            assert!(view.get_uncommitted_deltas().unwrap().is_empty());
        }
    }

    /// Cost above the supplied gas reverts out of gas, consuming everything and
    /// staging nothing.
    #[test]
    fn reverts_out_of_gas_when_cost_exceeds_supplied() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let create = Op::Create {
            key: [1u8; 32],
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            attributes: Vec::new(),
        };
        let cost = PlaceholderCost.op_cost(&create);
        assert!(cost > 0, "placeholder must charge for a create");

        let out = exec
            .apply(
                &env_gas([0xAA; 20], 10, cost - 1),
                &mut view,
                core::slice::from_ref(&create),
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Reverted);
        assert_eq!(out.gas_used(), cost - 1);
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
    }

    /// Running out of gas after an earlier op already wrote undoes that write:
    /// the entity, its minting-nonce step and its delta all go, no effect is
    /// reported, and no frame is left open.
    #[test]
    fn out_of_gas_after_a_write_undoes_it() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let alice = [0xAA; 20];
        let ops = [create_until(1, 50), create_until(2, 50)];
        let first = PlaceholderCost.op_cost(&ops[0]);
        let both = first + PlaceholderCost.op_cost(&ops[1]);

        let mut effects = Vec::new();
        let out = exec
            .apply_with_effects(&env_gas(alice, 10, both - 1), &mut view, &ops, &mut effects)
            .unwrap();

        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert!(
            staged(&view, [1u8; 32]).is_none(),
            "the first create is undone"
        );
        assert_eq!(minting_nonce(&view, alice), 0);
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
        assert!(effects.is_empty());
        assert_eq!(view.open_frames(), 0);
    }

    #[test]
    fn succeeds_when_gas_exactly_covers_cost() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        let create = Op::Create {
            key: [1u8; 32],
            expires_at: 50,
            creation_flags: CreationFlags::NONE,
            attributes: Vec::new(),
        };
        // Its op cost, and 40 blocks of storage rounded up to 1.
        let cost = PlaceholderCost.op_cost(&create) + 1;

        let out = exec
            .apply(
                &env_gas([0xAA; 20], 10, cost),
                &mut view,
                core::slice::from_ref(&create),
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas_used(), cost);
        assert_eq!(view.get_uncommitted_deltas().unwrap().len(), 1);
    }

    /// Undecodable calldata surfaces as a host fault, not a silent success.
    #[test]
    fn execute_rejects_undecodable_calldata() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
        assert!(matches!(
            exec.execute(&env([0u8; 20], 1), &mut view, &[0xDE, 0xAD]),
            Err(ExecError::Decode(_))
        ));
    }
}
