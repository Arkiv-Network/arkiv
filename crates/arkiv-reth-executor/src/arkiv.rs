//! Arkiv's entity executor — the business logic, host-agnostic. All-or-nothing:
//! ops stage into a private overlay and only reach the view (as net
//! [`EntityUpdates`]) if every one succeeds. A business-rule violation is a
//! *revert*, not an [`Err`] — errors are host/store faults.

use core::fmt;
use core::marker::PhantomData;
use std::collections::BTreeMap;

use arkiv_interfaces::entity::{Attribute, CreationFlags, Entity, annotations};
use arkiv_interfaces::execution::{
    AttributeMutation, ExecEnv, ExecOutput, ExecStatus, Op, OpKind, RevertReason,
    TransactionExecutor,
};
use arkiv_interfaces::gas::{CostModel, PlaceholderCost};
use arkiv_interfaces::primitives::{BlockNumber, EntityAddress, EntityCreationNonce, UserAddress};
use arkiv_interfaces::statemanager::{EntityUpdates, ReadMode, StateView};

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
    _state: PhantomData<fn() -> V>,
}

impl<V> ArkivExecutor<V, PlaceholderCost> {
    pub const fn new() -> Self {
        Self {
            cost: PlaceholderCost,
            _state: PhantomData,
        }
    }
}

impl<V, C> ArkivExecutor<V, C> {
    pub const fn with_cost(cost: C) -> Self {
        Self {
            cost,
            _state: PhantomData,
        }
    }
}

impl<V> Default for ArkivExecutor<V, PlaceholderCost> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V, C: Clone> Clone for ArkivExecutor<V, C> {
    fn clone(&self) -> Self {
        Self::with_cost(self.cost.clone())
    }
}

impl<V, C: fmt::Debug> fmt::Debug for ArkivExecutor<V, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArkivExecutor")
            .field("cost", &self.cost)
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
    pub fn apply_with_effects(
        &self,
        env: &ExecEnv,
        state: &mut V,
        ops: &[Op],
        effects: &mut Vec<OpEffect>,
    ) -> Result<ExecOutput, ExecError> {
        // Per-transaction overlay: Some(entity) = staged write, None = staged
        // delete. Reads consult it before the view, so operations in this
        // transaction see each other's effects.
        let mut tx_overlay = BTreeMap::<EntityAddress, Option<Entity>>::new();
        let mut gas_used = 0u64;
        let mut staged_effects = Vec::with_capacity(ops.len());

        for op in ops {
            // Charge up front. Outrunning the supplied gas consumes all of it
            // and stages nothing.
            gas_used = gas_used.saturating_add(self.cost.op_cost(op));
            if gas_used > env.gas_supplied {
                return Ok(ExecOutput {
                    status: ExecStatus::Reverted,
                    gas_used: env.gas_supplied,
                    revert: Some(RevertReason::OutOfGas),
                });
            }
            match self.stage_op(env, state, &mut tx_overlay, op)? {
                Ok(effect) => staged_effects.push(effect),
                Err(reason) => {
                    return Ok(ExecOutput {
                        status: ExecStatus::Reverted,
                        gas_used,
                        revert: Some(reason),
                    });
                }
            }
        }

        // Every op succeeded: write each touched entity's net change into the
        // view, as a partial update against what the view held before.
        for (key, staged) in tx_overlay {
            let before = state
                .get_entity(key, ReadMode::ViewWithOverlay)
                .map_err(ExecError::store)?;
            if let Some(updates) = updates_between(key, before.as_ref(), staged.as_ref()) {
                state.update_entity(updates).map_err(ExecError::store)?;
            }
        }

        effects.extend(staged_effects);
        Ok(ExecOutput {
            status: ExecStatus::Ok,
            gas_used,
            revert: None,
        })
    }

    /// Stage one operation into `overlay`. `Ok(Ok(effect))` staged it;
    /// `Ok(Err(reason))` is a business-rule revert; `Err(_)` a store fault.
    fn stage_op(
        &self,
        env: &ExecEnv,
        state: &V,
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
                if self.current(state, overlay, *key)?.is_some() {
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
                    overlay,
                    *key,
                    Auth::OwnerOnly,
                    // `readonly` freezes contents; lifecycle ops still work.
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
                    overlay,
                    *key,
                    // Anyone may pay to keep an entity alive when the flag says
                    // so — extending can't hurt the owner.
                    Auth::OwnerOrFlag(CreationFlags::PERMISSIONLESS_EXTENSION),
                    // Lifetimes never shorten; equal is a satisfied no-op.
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
                let previous_owner = self.current(state, overlay, *key)?.map(|e| e.owner);
                self.mutate(
                    env,
                    state,
                    overlay,
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
                let Some(entity) = self.current(state, overlay, *key)? else {
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

    /// Load, authorize, check liveness, run the op's own check, edit, re-stage.
    /// An expired entity is not mutable — including by extend.
    #[allow(clippy::too_many_arguments)]
    fn mutate(
        &self,
        env: &ExecEnv,
        state: &V,
        overlay: &mut BTreeMap<EntityAddress, Option<Entity>>,
        key: EntityAddress,
        auth: Auth,
        check: impl FnOnce(&Entity) -> Result<(), RevertReason>,
        edit: impl FnOnce(&mut Entity),
    ) -> Result<Result<(UserAddress, BlockNumber), RevertReason>, ExecError> {
        let Some(mut entity) = self.current(state, overlay, key)? else {
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

    fn current(
        &self,
        state: &V,
        overlay: &BTreeMap<EntityAddress, Option<Entity>>,
        key: EntityAddress,
    ) -> Result<Option<Entity>, ExecError> {
        if let Some(staged) = overlay.get(&key) {
            return Ok(staged.clone());
        }
        state
            .get_entity(key, ReadMode::ViewWithOverlay)
            .map_err(ExecError::store)
    }
}

/// The net `before → after` change: only the fields that differ. `None` when
/// nothing changed (create-then-delete nets to nothing).
fn updates_between(
    key: EntityAddress,
    before: Option<&Entity>,
    after: Option<&Entity>,
) -> Option<EntityUpdates> {
    match (before, after) {
        (None, None) => None,
        (Some(_), None) => Some(EntityUpdates::deletion(key)),
        (None, Some(a)) => Some(EntityUpdates::create(a.clone())),
        (Some(b), Some(a)) => {
            let updates = EntityUpdates {
                entity: key,
                delete: false,
                creator: (b.creator != a.creator).then_some(a.creator),
                owner: (b.owner != a.owner).then_some(a.owner),
                created_at_block: (b.created_at_block != a.created_at_block)
                    .then_some(a.created_at_block),
                last_modified_at_block: (b.last_modified_at_block != a.last_modified_at_block)
                    .then_some(a.last_modified_at_block),
                expires_at: (b.expires_at != a.expires_at).then_some(a.expires_at),
                creation_flags: (b.creation_flags != a.creation_flags).then_some(a.creation_flags),
                content_type: (b.content_type != a.content_type).then(|| a.content_type.clone()),
                payload: (b.payload != a.payload).then(|| a.payload.clone()),
                attributes: (b.attributes != a.attributes).then(|| a.attributes.clone()),
            };
            (updates
                != EntityUpdates {
                    entity: key,
                    ..EntityUpdates::default()
                })
            .then_some(updates)
        }
    }
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

/// Set some attributes, unset others, leave the rest untouched. `$payload` and
/// `$contentType` route to the entity's own fields; every other `$` name was
/// rejected at decode.
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
                    // A set replaces the value *and* its type.
                    Some(existing) => existing.value = value.clone(),
                    None => entity.attributes.push(Attribute {
                        key: name.to_vec(),
                        value: value.clone(),
                    }),
                },
                // Unsetting an unset attribute is a no-op, not an error.
                None => entity.attributes.retain(|a| a.key != name),
            },
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
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::statemanager::{BlockRef, EntityStore, StateView};
    use arkiv_reth_statemanager::MptStateView;
    use core::convert::Infallible;
    use std::collections::{HashMap, HashSet};

    use alloy_primitives::{Address, B256, U256};
    use arkiv_reth_mpt_committed_store::{AccountCode, BalanceAccess, IndexStorage, NonceAccess};

    /// An in-memory base implementing every raw seam the view needs.
    #[derive(Debug, Default, Clone)]
    struct MemState {
        balances: HashMap<Address, U256>,
        nonces: HashMap<Address, u64>,
        code: HashMap<Address, Vec<u8>>,
        slots: HashMap<(Address, B256), B256>,
        persisted: HashSet<Address>,
    }

    impl AccountCode for MemState {
        type Error = Infallible;
        fn code(&mut self, addr: Address) -> Result<Vec<u8>, Infallible> {
            Ok(self.code.get(&addr).cloned().unwrap_or_default())
        }
        fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Infallible> {
            self.code.insert(addr, code);
            Ok(())
        }
        fn clear_code(&mut self, addr: Address) -> Result<(), Infallible> {
            self.code.remove(&addr);
            Ok(())
        }
    }

    impl IndexStorage for MemState {
        type Error = Infallible;
        fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Infallible> {
            Ok(self.slots.get(&(addr, slot)).copied().unwrap_or(B256::ZERO))
        }
        fn set_storage(
            &mut self,
            addr: Address,
            slot: B256,
            value: B256,
        ) -> Result<(), Infallible> {
            self.slots.insert((addr, slot), value);
            Ok(())
        }
        fn ensure_account_persists(&mut self, addr: Address) -> Result<(), Infallible> {
            self.persisted.insert(addr);
            Ok(())
        }
    }

    impl BalanceAccess for MemState {
        type Error = Infallible;
        fn get_balance(&mut self, addr: Address) -> Result<U256, Infallible> {
            Ok(self.balances.get(&addr).copied().unwrap_or_default())
        }
        fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Infallible> {
            self.balances.insert(addr, balance);
            Ok(())
        }
    }

    impl NonceAccess for MemState {
        type Error = Infallible;
        fn get_nonce(&mut self, addr: Address) -> Result<u64, Infallible> {
            Ok(self.nonces.get(&addr).copied().unwrap_or_default())
        }
        fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Infallible> {
            self.nonces.insert(addr, nonce);
            Ok(())
        }
    }

    type View = MptStateView<MemState>;

    fn fresh_view() -> View {
        View::new(MemState::default(), BlockRef::new(9, [0xBB; 32]))
    }

    /// A view whose base already holds `entities`.
    fn seeded_view(entities: &[Entity]) -> View {
        let mut view = fresh_view();
        for entity in entities {
            view.update_entity(EntityUpdates::create(entity.clone()))
                .unwrap();
        }
        StateView::commit(&mut view).unwrap();
        View::new(view.into_base(), BlockRef::new(9, [0xBB; 32]))
    }

    fn staged(view: &View, key: EntityAddress) -> Option<Entity> {
        view.get_entity(key, ReadMode::ViewWithOverlay).unwrap()
    }

    /// Comfortably more gas than any op here costs.
    const AMPLE_GAS: u64 = 100_000_000;

    fn env(caller: [u8; 20], block: u64) -> ExecEnv {
        env_gas(caller, block, AMPLE_GAS)
    }

    fn env_gas(caller: [u8; 20], block: u64, gas_supplied: u64) -> ExecEnv {
        ExecEnv {
            caller,
            block_number: block,
            gas_supplied,
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
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: b"hello".to_vec(),
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
                    content_type: b"x".to_vec(),
                    payload: b"y".to_vec(),
                    attributes: Vec::new(),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        let e = staged(&view, [1u8; 32]).expect("staged in the view overlay");
        assert_eq!(e.owner, alice);
        assert_eq!(e.creator, alice);
        assert_eq!(e.created_at_block, 10);
        // Not on the base until commit.
        assert!(
            view.get_entity([1u8; 32], ReadMode::ViewOnBase)
                .unwrap()
                .is_none()
        );
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
                    content_type: Vec::new(),
                    payload: Vec::new(),
                    attributes: Vec::new(),
                }],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Reverted);
        assert!(out.revert.is_some());
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
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
        assert_eq!(e.last_modified_at_block, 20);
        // The staged delta is partial: only what the transfer changed.
        let deltas = view.get_uncommitted_deltas().unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].owner.is_some());
        assert!(deltas[0].payload.is_none());
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

    /// Ops within one transaction see each other; create-then-delete nets to
    /// nothing at all.
    #[test]
    fn create_then_delete_in_one_tx_nets_to_nothing() {
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
                        content_type: Vec::new(),
                        payload: Vec::new(),
                        attributes: Vec::new(),
                    },
                    Op::Delete { key: [3u8; 32] },
                ],
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
        assert!(staged(&view, [3u8; 32]).is_none());
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

    /// A patch replaces contents, stamps `last_modified_at_block`, and leaves
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
        let e = staged(&view, [7u8; 32]).unwrap();
        assert_eq!(e.content_type, b"application/json");
        assert_eq!(e.payload, b"world");
        assert_eq!(e.attributes.len(), 2, "unmentioned attributes survive");
        assert_eq!(e.last_modified_at_block, 20);
        assert_eq!(e.creator, [1u8; 20]);
        assert_eq!(e.owner, [2u8; 20]);
        assert_eq!(e.created_at_block, 3);
        assert_eq!(e.expires_at, 100);

        let deltas = view.get_uncommitted_deltas().unwrap();
        assert!(deltas[0].content_type.is_some());
        assert!(deltas[0].payload.is_some());
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
            content_type: Vec::new(),
            payload: Vec::new(),
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
        assert_eq!(out.gas_used, cost - 1);
        assert_eq!(out.revert, Some(RevertReason::OutOfGas));
        assert!(view.get_uncommitted_deltas().unwrap().is_empty());
    }

    #[test]
    fn succeeds_when_gas_exactly_covers_cost() {
        let exec = ArkivExecutor::<View>::new();
        let mut view = fresh_view();
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
                &env_gas([0xAA; 20], 10, cost),
                &mut view,
                core::slice::from_ref(&create),
            )
            .unwrap();

        assert_eq!(out.status, ExecStatus::Ok);
        assert_eq!(out.gas_used, cost);
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
