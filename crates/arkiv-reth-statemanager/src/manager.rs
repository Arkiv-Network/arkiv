//! [`MptStateManager`] — every Arkiv state lane, multiplexed onto Ethereum's
//! one MPT account state.
//!
//! The manager holds no lane logic of its own: it **composes** the per-lane
//! stores — the committed ones from `arkiv-reth-mpt-committed-store`, the
//! uncommitted one from `arkiv-reth-uncommitted-store` — and directs every
//! `arkiv_interfaces::manager` call at the right one. A call borrows the base
//! into the store for its duration, so the manager can never drift from the
//! standalone stores:
//!
//! | lane                  | store                                     | seam of `B`       |
//! |-----------------------|-------------------------------------------|-------------------|
//! | entities              | [`RethEntityStore`] (via [`CodeBackend`]) | [`AccountCode`]   |
//! | query index           | [`RethAuxStore`]                          | code + slots      |
//! | balances              | [`RethAccountBalancesStore`]              | [`BalanceAccess`] |
//! | transaction nonces    | [`RethAccountNoncesStore`]                | [`NonceAccess`]   |
//! | entity-minting nonces | [`RethEntityCreationNoncesStore`]         | [`IndexStorage`]  |
//! | pruning map           | [`MemPruningMap`] — owned; node-local, uncommitted | —        |
//!
//! The multiplexing is the point: a consumer sees six tidy lanes, while the
//! host sees one state trie and one unified state root. Swap the trie for
//! something custom later and only the store crates change — the
//! `StateManager` API holds still.
//!
//! ## Pricing
//!
//! [`StateManager::get_operation_cost`] is answered here too: the manager holds
//! a [`CostModel`] schedule (`C`, [`PlaceholderCost`] until the real numbers
//! land) and prices deterministically with it. Today's schedule consults no
//! state; the `&mut self` signature already permits a future schedule to read
//! any committed lane on the way to an answer.
//!
//! ## Shallow copies
//!
//! [`StateManager::shallow_copy`] is `Clone`: the pruning map and cost schedule
//! are copied, and the base is cloned — so the impl exists only for `B: Clone`,
//! and the base's own `Clone` must mean "share the committed state, copy any
//! private overlay". An in-memory or snapshot-backed base has exactly that
//! shape. The live write base ([`WriteOverlay`](crate::WriteOverlay)) holds
//! `&mut` exclusivity over reth's `Database` and is deliberately *not* `Clone`:
//! within one transaction there is nothing to fork, and simulate-vs-build forks
//! happen at the block level, over per-block state snapshots.
//!
//! ## Commitments and rewind
//!
//! On this host both store commitments answer the all-zero [`Hash`](type@Hash): every lane
//! lands in the one state trie, so the only real commitment is the block's
//! unified state root, which reth computes at seal time. Likewise
//! [`StateManager::rewind_to`] is refused — reth rewinds its own trie on a
//! reorg; rewinding *here* would mean lying about state we don't own.

use arkiv_interfaces::entity::Entity;
use arkiv_interfaces::execution::Op;
use arkiv_interfaces::gas::{CostModel, PlaceholderCost};
use arkiv_interfaces::manager::{
    AccountBalancesStore, AccountNoncesStore, EntityCreationNoncesStore, PruningMap,
    PruningPriority, StateManager,
};
use arkiv_interfaces::primitives::{
    BlockNumber, EntityAddress, EntityCreationNonce, Gas, Hash, UserAddress, UserBalance, UserNonce,
};
use arkiv_interfaces::query::{PageParams, Query, QueryMatches, QueryStats};
use arkiv_interfaces::state::{
    AuxiliaryStore, BlockAuxiliaryStoreDelta, BlockEntityStoreDelta, EntityStore,
};
use arkiv_reth_mpt_committed_store::{
    AccountCode, AuxError, BalanceAccess, CodeBackend, CodeBackendError, IndexStorage, NonceAccess,
    RethAccountBalancesStore, RethAccountNoncesStore, RethAuxStore, RethEntityCreationNoncesStore,
    RethEntityStore,
};
use arkiv_reth_uncommitted_store::MemPruningMap;

/// What can go wrong in an [`MptStateManager`], generic over the base's own
/// error `E`.
#[derive(Debug)]
pub enum MptError<E> {
    /// A raw seam read or write failed (the account or nonce lanes).
    Backend(E),
    /// The entity lane failed (a backend fault, or stored bytes that aren't a
    /// valid entity record).
    Entity(CodeBackendError<E>),
    /// The index lane failed (a backend fault, or a corrupt stored bitmap).
    Index(AuxError<E>),
    /// The operation isn't available on this host.
    Unsupported(&'static str),
}

/// The reth-host [`StateManager`]: the per-lane stores composed over one
/// Ethereum account state, reached through base `B`'s raw seams.
///
/// `B` is whatever holds that state: the write overlay for the transaction
/// path, a provider snapshot for reads, an in-memory mock for tests. `C` is the
/// cost schedule the manager prices with ([`PlaceholderCost`] until the real
/// schedule lands).
#[derive(Debug, Clone)]
pub struct MptStateManager<B, C = PlaceholderCost> {
    base: B,
    costs: C,
    pruning: MemPruningMap,
}

impl<B> MptStateManager<B> {
    /// A manager over `base` with the placeholder cost schedule.
    pub const fn new(base: B) -> Self {
        Self::with_cost_model(base, PlaceholderCost)
    }
}

impl<B, C> MptStateManager<B, C> {
    /// A manager over `base` pricing with `costs`.
    pub const fn with_cost_model(base: B, costs: C) -> Self {
        Self {
            base,
            costs,
            pruning: MemPruningMap::new(),
        }
    }

    /// The cost schedule the manager prices with — host wiring for paths that
    /// need the schedule object itself (the executor charges per op mid-batch);
    /// consumers ask [`StateManager::get_operation_cost`] instead.
    pub const fn cost_model(&self) -> &C {
        &self.costs
    }

    /// The underlying base.
    pub const fn base(&self) -> &B {
        &self.base
    }

    /// Unwrap the base — how the write path takes back its overlay (and the
    /// staged diff inside it) once a transaction is done with the manager.
    pub fn into_base(self) -> B {
        self.base
    }
}

// ── Committed lanes ───────────────────────────────────────────────────────

impl<B, C, E> EntityStore for MptStateManager<B, C>
where
    B: AccountCode<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get(&mut self, entity: EntityAddress) -> Result<Option<Entity>, Self::Error> {
        RethEntityStore::new(CodeBackend::new(&mut self.base))
            .get(entity)
            .map_err(MptError::Entity)
    }

    fn apply_delta(&mut self, delta: &BlockEntityStoreDelta) -> Result<(), Self::Error> {
        RethEntityStore::new(CodeBackend::new(&mut self.base))
            .apply_delta(delta)
            .map_err(MptError::Entity)
    }

    /// The all-zero [`Hash`](type@Hash): on this host the entities commit through the
    /// block's unified state root, computed by reth — there is no store-scoped
    /// sub-commitment to answer with (see the module docs).
    fn commitment(&mut self) -> Result<Hash, Self::Error> {
        Ok(Hash::default())
    }
}

impl<B, C, E> AuxiliaryStore for MptStateManager<B, C>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn evaluate(&mut self, query: &Query, page: PageParams) -> Result<QueryMatches, Self::Error> {
        RethAuxStore::new(&mut self.base)
            .evaluate(query, page)
            .map_err(MptError::Index)
    }

    fn apply_delta(&mut self, delta: &BlockAuxiliaryStoreDelta) -> Result<(), Self::Error> {
        RethAuxStore::new(&mut self.base)
            .apply_delta(delta)
            .map_err(MptError::Index)
    }

    /// The all-zero [`Hash`](type@Hash), for the same reason as
    /// [`EntityStore::commitment`] above.
    fn commitment(&mut self) -> Result<Hash, Self::Error> {
        Ok(Hash::default())
    }
}

impl<B, C, E> AccountBalancesStore for MptStateManager<B, C>
where
    B: BalanceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_balance(&mut self, account: UserAddress) -> Result<UserBalance, Self::Error> {
        RethAccountBalancesStore::new(&mut self.base)
            .get_balance(account)
            .map_err(MptError::Backend)
    }

    fn set_balance(
        &mut self,
        account: UserAddress,
        balance: UserBalance,
    ) -> Result<(), Self::Error> {
        RethAccountBalancesStore::new(&mut self.base)
            .set_balance(account, balance)
            .map_err(MptError::Backend)
    }
}

impl<B, C, E> AccountNoncesStore for MptStateManager<B, C>
where
    B: NonceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_account_nonce(&mut self, account: UserAddress) -> Result<UserNonce, Self::Error> {
        RethAccountNoncesStore::new(&mut self.base)
            .get_account_nonce(account)
            .map_err(MptError::Backend)
    }

    fn set_account_nonce(
        &mut self,
        account: UserAddress,
        nonce: UserNonce,
    ) -> Result<(), Self::Error> {
        RethAccountNoncesStore::new(&mut self.base)
            .set_account_nonce(account, nonce)
            .map_err(MptError::Backend)
    }
}

impl<B, C, E> EntityCreationNoncesStore for MptStateManager<B, C>
where
    B: IndexStorage<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn get_entity_nonce(&mut self, owner: UserAddress) -> Result<EntityCreationNonce, Self::Error> {
        RethEntityCreationNoncesStore::new(&mut self.base)
            .get_entity_nonce(owner)
            .map_err(MptError::Backend)
    }

    fn advance_entity_nonce(
        &mut self,
        owner: UserAddress,
        by: u64,
    ) -> Result<EntityCreationNonce, Self::Error> {
        RethEntityCreationNoncesStore::new(&mut self.base)
            .advance_entity_nonce(owner, by)
            .map_err(MptError::Backend)
    }
}

// ── Uncommitted lane ──────────────────────────────────────────────────────

/// Delegates to the owned [`MemPruningMap`]. Its error is `Infallible`, so the
/// `map_err` arms are empty matches — pure type conversion.
impl<B, C, E> PruningMap for MptStateManager<B, C>
where
    B: BalanceAccess<Error = E>,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    fn schedule_pruning(
        &mut self,
        entity: EntityAddress,
        prune_at: BlockNumber,
        priority: PruningPriority,
    ) -> Result<(), Self::Error> {
        self.pruning
            .schedule_pruning(entity, prune_at, priority)
            .map_err(|e| match e {})
    }

    fn pruning_due(&mut self, block: BlockNumber) -> Result<Vec<EntityAddress>, Self::Error> {
        self.pruning.pruning_due(block).map_err(|e| match e {})
    }

    fn clear_pruning(&mut self, entities: &[EntityAddress]) -> Result<(), Self::Error> {
        self.pruning.clear_pruning(entities).map_err(|e| match e {})
    }
}

// ── The umbrella ──────────────────────────────────────────────────────────

impl<B, C, E> StateManager for MptStateManager<B, C>
where
    B: AccountCode<Error = E>
        + IndexStorage<Error = E>
        + BalanceAccess<Error = E>
        + NonceAccess<Error = E>
        + Clone,
    C: CostModel + Clone,
    E: core::fmt::Debug,
{
    type Error = MptError<E>;

    /// Priced by the carried schedule. State-independent today; see the module
    /// docs for why the signature already allows consulting the stores.
    fn get_operation_cost(&mut self, op: &Op) -> Result<Gas, MptError<E>> {
        Ok(self.costs.op_cost(op))
    }

    fn get_query_cost(&mut self, stats: &QueryStats) -> Result<Gas, MptError<E>> {
        Ok(self.costs.query_cost(stats))
    }

    fn shallow_copy(&self) -> Self {
        self.clone()
    }

    /// Refused on this host — reth reorgs its own trie; see the module docs.
    fn rewind_to(&mut self, _block: BlockNumber) -> Result<(), MptError<E>> {
        Err(MptError::Unsupported(
            "the reth host rewinds committed state itself on a reorg; \
             build a manager over the target block's state instead",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use std::collections::{HashMap, HashSet};

    use alloy_primitives::{Address, B256, U256};
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::entity::annotations::{ALL, OWNER};
    use arkiv_interfaces::execution::BlockDraft;
    use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn};
    use arkiv_interfaces::state::{AttrEntry, AuxiliaryEntityDelta};
    use arkiv_reth_mpt_committed_store::entities::layout::{
        SYSTEM_ACCOUNT_ADDRESS, entity_leaf_address, nonce_slot,
    };

    /// An in-memory base implementing every raw seam — the shape any real base
    /// (write overlay, snapshot) has, and `Clone` in exactly the way a shallow
    /// copy needs.
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

    type Mgr = MptStateManager<MemState>;

    fn key_of(byte: u8) -> EntityAddress {
        [byte; 32]
    }

    fn entity_of(byte: u8, owner: u8) -> Entity {
        Entity {
            key: key_of(byte),
            owner: [owner; 20],
            expires_at: 100,
            payload: b"hi".to_vec(),
            ..Entity::default()
        }
    }

    /// The index delta a create stages: the `$all` marker plus the `$owner`
    /// pair — enough index to answer the queries these tests ask.
    fn indexed_create(byte: u8, owner: u8) -> AuxiliaryEntityDelta {
        AuxiliaryEntityDelta {
            entity_key: key_of(byte),
            inserts: vec![
                AttrEntry::new(ALL, AttributeValue::Str(String::new())),
                AttrEntry::new(OWNER, AttributeValue::EthereumAddress([owner; 20])),
            ],
            removes: Vec::new(),
        }
    }

    fn put(mgr: &mut Mgr, entity: Entity) {
        EntityStore::apply_delta(
            mgr,
            &BlockEntityStoreDelta {
                puts: vec![entity],
                deletes: Vec::new(),
            },
        )
        .unwrap();
    }

    fn owned_by(mgr: &mut Mgr, owner: u8) -> Vec<EntityAddress> {
        let query = Query::Eq {
            key: AnnotKey::BuiltIn(BuiltIn::Owner),
            value: AnnotVal::EthereumAddress([owner; 20]),
        };
        let page = PageParams {
            page_size: 100,
            cursor: None,
        };
        let mut keys = mgr.evaluate(&query, page).unwrap().keys;
        keys.sort();
        keys
    }

    /// The entity lane really multiplexes onto account code: a put through the
    /// manager lands as code at the entity's derived account address.
    #[test]
    fn entities_multiplex_onto_account_code() {
        let mut mgr = Mgr::new(MemState::default());
        let key = key_of(7);

        assert!(mgr.get(key).unwrap().is_none());
        put(&mut mgr, entity_of(7, 2));

        assert_eq!(mgr.get(key).unwrap(), Some(entity_of(7, 2)));
        assert!(mgr.base().code.contains_key(&entity_leaf_address(key)));

        EntityStore::apply_delta(
            &mut mgr,
            &BlockEntityStoreDelta {
                puts: Vec::new(),
                deletes: vec![key],
            },
        )
        .unwrap();
        assert!(mgr.get(key).unwrap().is_none());
    }

    /// The index lane answers queries over deltas applied through the manager —
    /// same state, same base, no separate store object in sight.
    #[test]
    fn index_lane_answers_owner_queries() {
        let mut mgr = Mgr::new(MemState::default());
        AuxiliaryStore::apply_delta(
            &mut mgr,
            &BlockAuxiliaryStoreDelta {
                entities: vec![indexed_create(0xA0, 1), indexed_create(0xB0, 2)],
            },
        )
        .unwrap();

        assert_eq!(owned_by(&mut mgr, 1), vec![key_of(0xA0)]);
        assert_eq!(owned_by(&mut mgr, 2), vec![key_of(0xB0)]);
        assert_eq!(owned_by(&mut mgr, 9), Vec::<EntityAddress>::new());
    }

    /// Balances and tx nonces read through to the base and write back to it, in
    /// spec types at the trait boundary.
    #[test]
    fn balances_and_nonces_reach_the_accounts() {
        let mut base = MemState::default();
        let alice = Address::repeat_byte(0xAA);
        base.balances.insert(alice, U256::from(1_000u64));
        base.nonces.insert(alice, 4);

        let mut mgr = Mgr::new(base);
        let alice_key = alice.into_array();
        assert_eq!(
            mgr.get_balance(alice_key).unwrap(),
            UserBalance::from_u64(1_000)
        );
        assert_eq!(mgr.get_account_nonce(alice_key).unwrap(), UserNonce::new(4));

        mgr.set_balance(alice_key, UserBalance::from_u64(250))
            .unwrap();
        mgr.set_account_nonce(alice_key, UserNonce::new(5)).unwrap();
        assert_eq!(mgr.base().balances[&alice], U256::from(250u64));
        assert_eq!(mgr.base().nonces[&alice], 5);

        // An untouched account reads as empty, not as an error.
        assert_eq!(mgr.get_balance([0xBB; 20]).unwrap(), UserBalance::ZERO);
        assert_eq!(mgr.get_account_nonce([0xBB; 20]).unwrap(), UserNonce::ZERO);
    }

    /// The minting-nonce lane is wired to the system account — the manager
    /// composes `RethEntityCreationNoncesStore` over the same base as
    /// everything else (the detailed slot semantics are that store's own
    /// tests).
    #[test]
    fn entity_nonces_live_on_the_system_account() {
        let mut mgr = Mgr::new(MemState::default());
        let alice: UserAddress = [0xAA; 20];

        assert_eq!(
            mgr.get_entity_nonce(alice).unwrap(),
            EntityCreationNonce::ZERO
        );
        assert_eq!(
            mgr.advance_entity_nonce(alice, 2).unwrap(),
            EntityCreationNonce::ZERO
        );
        assert_eq!(
            mgr.get_entity_nonce(alice).unwrap(),
            EntityCreationNonce::new(2)
        );

        assert!(mgr.base().persisted.contains(&SYSTEM_ACCOUNT_ADDRESS));
        assert!(
            mgr.base()
                .slots
                .contains_key(&(SYSTEM_ACCOUNT_ADDRESS, nonce_slot(Address::from(alice))))
        );
    }

    /// The core promise: a shallow copy diverges freely — writes on either side
    /// are invisible to the other — while everything from before the fork is
    /// visible in both. Drop the copy to have simulated; keep it to have built.
    #[test]
    fn shallow_copies_are_independent() {
        let mut mgr = Mgr::new(MemState::default());
        put(&mut mgr, entity_of(1, 1));
        mgr.set_balance([0xAA; 20], UserBalance::from_u64(100))
            .unwrap();
        mgr.schedule_pruning(key_of(1), 50, 0).unwrap();

        let mut copy = mgr.shallow_copy();

        // Pre-fork state is visible through the copy.
        assert_eq!(copy.get(key_of(1)).unwrap(), Some(entity_of(1, 1)));
        assert_eq!(
            copy.get_balance([0xAA; 20]).unwrap(),
            UserBalance::from_u64(100)
        );
        assert_eq!(copy.pruning_due(50).unwrap(), vec![key_of(1)]);

        // The copy diverges: new entity, spent balance, extra tombstone.
        put(&mut copy, entity_of(2, 1));
        copy.set_balance([0xAA; 20], UserBalance::ZERO).unwrap();
        copy.schedule_pruning(key_of(2), 60, 0).unwrap();

        // The original never sees any of it…
        assert!(mgr.get(key_of(2)).unwrap().is_none());
        assert_eq!(
            mgr.get_balance([0xAA; 20]).unwrap(),
            UserBalance::from_u64(100)
        );
        assert_eq!(mgr.pruning_due(60).unwrap(), vec![key_of(1)]);

        // …and the original's later writes never reach the copy.
        put(&mut mgr, entity_of(3, 1));
        assert!(copy.get(key_of(3)).unwrap().is_none());
    }

    /// The umbrella: one `StateManager` bound applies a whole draft (both
    /// lanes), answers pricing questions deterministically, and refuses to
    /// rewind on this host.
    #[test]
    fn state_manager_umbrella_applies_drafts_prices_and_refuses_rewind() {
        fn drive<M: StateManager>(mgr: &mut M, draft: &BlockDraft, op: &Op) {
            mgr.apply_draft(draft).unwrap();
            // Pricing is asked of the manager, and the answer is deterministic.
            let first = mgr.get_operation_cost(op).unwrap();
            assert!(first > 0, "a delete op has a non-zero base cost");
            assert_eq!(mgr.get_operation_cost(op).unwrap(), first);
            assert_eq!(mgr.get_query_cost(&QueryStats::default()).unwrap(), 0);
            // This host refuses to rewind; others may not.
            assert!(mgr.rewind_to(0).is_err());
        }

        let mut mgr = Mgr::new(MemState::default());
        let draft = BlockDraft {
            entities: BlockEntityStoreDelta {
                puts: vec![entity_of(0xA0, 1)],
                deletes: Vec::new(),
            },
            auxiliary: BlockAuxiliaryStoreDelta {
                entities: vec![indexed_create(0xA0, 1)],
            },
        };
        drive(&mut mgr, &draft, &Op::Delete { key: key_of(0xA0) });

        assert_eq!(mgr.get(key_of(0xA0)).unwrap(), Some(entity_of(0xA0, 1)));
        assert_eq!(owned_by(&mut mgr, 1), vec![key_of(0xA0)]);
        assert!(matches!(mgr.rewind_to(3), Err(MptError::Unsupported(_))));
    }
}
