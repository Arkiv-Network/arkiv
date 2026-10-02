//! Materialising a [`SeedSpec`] into account state, through the node's own
//! write path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_interfaces::entity::{Entity, annotations};
use arkiv_interfaces::execution::{ExecEnv, ExecStatus, Op};
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::statemanager::{BlockRef, EntityCreationNoncesStore, EntityStore, StateView};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_reth_mpt_committed_store::entities::layout::{
    SYSTEM_ACCOUNT_ADDRESS, entity_leaf_address,
};
use arkiv_reth_mpt_committed_store::{
    AuxiliaryEntityDelta, RethAuxStore, annotation_delta, id_to_key_slot, key_to_id_slot,
    pair_address,
};
use arkiv_reth_statemanager::write_manager;
use eyre::{Result, WrapErr, bail};
use reth_ethereum::evm::revm::{
    DatabaseCommit,
    database_interface::EmptyDB,
    db::{AccountState, CacheDB, DbAccount},
    primitives::KECCAK_EMPTY,
};

use crate::manifest::SeedManifest;
use crate::sink::{AccountSink, MemorySink};
use crate::spec::SeedSpec;

/// The accounts a seed produced in memory, plus the record of what was seeded.
#[derive(Debug, Clone)]
pub struct SeededState {
    /// Every account of the genesis: entity records, index buckets, the
    /// system account, and the base alloc merged in.
    pub alloc: BTreeMap<Address, GenesisAccount>,
    /// What was built; its `state_root` and `accounts` describe `alloc`.
    pub manifest: SeedManifest,
}

/// A progress report after each committed batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Entities committed so far.
    pub done: u64,
    /// Entities the spec asks for.
    pub total: u64,
    /// Milliseconds since the build started.
    pub elapsed_ms: u128,
    /// Accounts still held in the cache after the batch: the seed's shape
    /// (index buckets, range nodes, the system account), not its size.
    pub cached_accounts: usize,
    /// Storage slots held across those accounts.
    pub cached_slots: usize,
}

/// Build the state `spec` describes in memory, `base` (funding, predeploys)
/// merged in. For seeds a machine can hold; [`build`] with a
/// [`StreamSink`](crate::sink::StreamSink) has no such limit.
pub fn build_in_memory(
    spec: &SeedSpec,
    base: BTreeMap<Address, GenesisAccount>,
    on_progress: impl FnMut(Progress),
) -> Result<SeededState> {
    let mut sink = MemorySink::default();
    let manifest = build(spec, base, &mut sink, on_progress)?;
    Ok(SeededState {
        alloc: sink.into_alloc(),
        manifest,
    })
}

/// Build the state `spec` describes, handing every account to `sink` as it is
/// finished, then `base` (funding, predeploys — an address both sides claim
/// is an error: a seed never overwrites an operator's account).
///
/// Batches of `spec.batch_size` creates run through the executor into a
/// `MptStateView` over the production `WriteOverlay`, whose base is revm's
/// in-memory `CacheDB`. Each batch commits like a block would — entities as
/// account code, then the index, then the owners' minting nonces — and the
/// resulting diff is folded into the cache. What the batch finished for good
/// leaves the cache for the sink right away: the entity records, and the
/// system account's key ↔ id slots. What stays is what later batches still
/// write — index buckets, range-index nodes, the counters — so the cache holds
/// the seed's *shape*, not its size. `on_progress` is called after every batch.
pub fn build(
    spec: &SeedSpec,
    base: BTreeMap<Address, GenesisAccount>,
    sink: &mut impl AccountSink,
    mut on_progress: impl FnMut(Progress),
) -> Result<SeedManifest> {
    spec.validate()?;
    let started = Instant::now();
    let mut db = CacheDB::new(EmptyDB::default());
    let mut sample_keys = Vec::with_capacity(SeedManifest::SAMPLE_KEYS.min(spec.count as usize));

    let mut next = 0u64;
    while next < spec.count {
        let end = (next + spec.batch_size as u64).min(spec.count);

        // One create per entity, grouped by owner: the executor runs a batch
        // under a single caller, and every owner's nonce advances by its share.
        let mut by_owner: BTreeMap<Address, Vec<Op>> = BTreeMap::new();
        let mut keys_in_order = Vec::with_capacity((end - next) as usize);
        for i in next..end {
            let op = spec.create_op(i)?;
            if sample_keys.len() < SeedManifest::SAMPLE_KEYS {
                sample_keys.push(B256::from(*op.key()));
            }
            keys_in_order.push(*op.key());
            by_owner.entry(spec.owner_of(i)).or_default().push(op);
        }

        let mut view = write_manager(&mut db, BlockRef::new(0, [0; 32]));
        // The executor is typed by the view it runs over, whose lifetime is
        // this batch's borrow of the cache — so one per batch.
        let executor = ArkivExecutor::new();
        for (owner, ops) in &by_owner {
            let env = ExecEnv {
                caller: owner.into_array(),
                block_number: 0,
                gas_supplied: u64::MAX,
                chain_id: spec.chain_id,
            };
            let out = executor
                .apply(&env, &mut view, ops)
                .map_err(|e| eyre::eyre!("seed batch at entity {next}: {e}"))?;
            if out.status != ExecStatus::Ok {
                bail!(
                    "seed batch at entity {next} reverted: {}",
                    out.revert
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "no reason".into())
                );
            }
            for _ in ops {
                view.fetch_increment_entity_creation_nonce(env.caller)
                    .map_err(|e| eyre::eyre!("advance minting nonce: {e:?}"))?;
            }
        }

        // The index deltas of this batch's creates — derived exactly as the
        // node derives them, then folded in bulk after the entities commit. In
        // seed order, so entity `i` is allocated index id `i` whatever the
        // batch size: the `$all` bitmap is one contiguous run, and the state
        // does not depend on how the seed was chunked.
        let mut staged: HashMap<_, _> = view
            .get_uncommitted_deltas()
            .map_err(|e| eyre::eyre!("staged entities: {e:?}"))?
            .into_iter()
            .map(|updates| (updates.entity, updates))
            .collect();
        let deltas: Vec<AuxiliaryEntityDelta> = keys_in_order
            .iter()
            .map(|key| {
                let updates = staged
                    .remove(key)
                    .expect("every create in the batch is staged");
                let mut entity = Entity::default();
                updates.apply_to(&mut entity);
                annotation_delta(*key, None, Some(&entity))
                    .expect("a create always adds annotations")
            })
            .collect();
        // The `$key` index has one bucket per entity, written once: it leaves
        // with the entity. Every other bucket is shared across entities.
        let key_buckets: Vec<Address> = deltas
            .iter()
            .flat_map(|delta| delta.inserts.iter())
            .filter(|entry| entry.attr == annotations::KEY)
            .map(|entry| {
                pair_address(
                    &entry.attr,
                    entry.value.attr_type(),
                    &entry.value.index_bytes(),
                )
            })
            .collect();
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("commit seed batch: {e:?}"))?;
        RethAuxStore::new(view.backend_mut())
            .apply_inserts_bulk(&deltas)
            .map_err(|e| eyre::eyre!("index seed batch: {e:?}"))?;
        let state = view.into_base().into_state();
        db.commit(state);
        release_batch(&mut db, &keys_in_order, &key_buckets, next, sink)
            .wrap_err_with(|| format!("hand over seed batch at entity {next}"))?;
        next = end;
        on_progress(Progress {
            done: next,
            total: spec.count,
            elapsed_ms: started.elapsed().as_millis(),
            cached_accounts: db.cache.accounts.len(),
            cached_slots: db.cache.accounts.values().map(|a| a.storage.len()).sum(),
        });
    }

    // What the seed still holds — index buckets, range nodes, the system
    // account with its counters — then the base, then the root.
    let remaining: Vec<Address> = db.cache.accounts.keys().copied().collect();
    for address in remaining {
        let account = db.cache.accounts.remove(&address).expect("listed");
        if let Some(genesis) = genesis_account(address, &account, &db)? {
            sink.account(address, genesis)?;
        }
    }
    for (address, account) in base {
        sink.account(address, account)
            .wrap_err("merge the base alloc (is an address both funded and seeded?)")?;
    }
    let finished = sink.finish()?;

    Ok(SeedManifest {
        chain_id: spec.chain_id,
        count: spec.count,
        payload_size: spec.payload_size,
        content_type: spec.content_type.clone(),
        owners: spec.owners.clone(),
        expires_at: spec.expires_at,
        attributes: spec.attributes.iter().map(ToString::to_string).collect(),
        seed: spec.seed,
        state_root: finished.state_root,
        accounts: finished.accounts,
        sample_keys,
        owner_nonces: spec.owner_nonces(),
    })
}

/// Hand a committed batch's finished state to the sink and drop it from the
/// cache: each entity's record account, its `$key` index bucket, and the
/// system account's two bookkeeping slots for it (key → id, id → key; entity
/// `i` holds id `i`), none of which any later batch reads or writes. Then drop
/// the bytecodes no remaining account refers to — the shared index buckets
/// are rewritten every batch, and revm's cache would otherwise keep every
/// version.
fn release_batch(
    db: &mut CacheDB<EmptyDB>,
    keys: &[EntityAddress],
    key_buckets: &[Address],
    first_id: u64,
    sink: &mut impl AccountSink,
) -> Result<()> {
    for address in key_buckets {
        let Some(account) = db.cache.accounts.remove(address) else {
            bail!("no `$key` bucket at {address}");
        };
        let Some(genesis) = genesis_account(*address, &account, db)? else {
            bail!("an empty `$key` bucket at {address}");
        };
        sink.account(*address, genesis)?;
    }
    for (offset, key) in keys.iter().enumerate() {
        let address = entity_leaf_address(*key);
        let Some(account) = db.cache.accounts.remove(&address) else {
            bail!("entity {key:?} left no account at {address}");
        };
        let Some(genesis) = genesis_account(address, &account, db)? else {
            bail!("entity {key:?} left an empty account at {address}");
        };
        sink.account(address, genesis)?;

        let id = first_id + offset as u64;
        if let Some(system) = db.cache.accounts.get_mut(&SYSTEM_ACCOUNT_ADDRESS) {
            for slot in [key_to_id_slot(*key), id_to_key_slot(id)] {
                if let Some(value) = system.storage.remove(&U256::from_be_bytes(slot.0)) {
                    sink.storage_part(SYSTEM_ACCOUNT_ADDRESS, slot, B256::from(value))?;
                }
            }
        }
    }
    let live: HashSet<B256> = db
        .cache
        .accounts
        .values()
        .map(|account| account.info.code_hash)
        .collect();
    db.cache
        .contracts
        .retain(|hash, _| hash.is_zero() || *hash == KECCAK_EMPTY || live.contains(hash));
    Ok(())
}

/// A cached account as a genesis account — nonce, balance, code, and the
/// non-zero storage slots — or `None` for one that was only ever read, or
/// written back to nothing.
fn genesis_account(
    address: Address,
    account: &DbAccount,
    db: &CacheDB<EmptyDB>,
) -> Result<Option<GenesisAccount>> {
    if account.account_state == AccountState::NotExisting {
        return Ok(None);
    }
    let info = &account.info;
    let code: Option<Bytes> = if info.code_hash == KECCAK_EMPTY {
        None
    } else {
        let bytecode = info
            .code
            .as_ref()
            .or_else(|| db.cache.contracts.get(&info.code_hash))
            .ok_or_else(|| {
                eyre::eyre!("account {address}: code {} not in cache", info.code_hash)
            })?;
        Some(bytecode.original_bytes())
    };
    // A slot written back to zero is, in Ethereum state, an absent slot.
    let storage: BTreeMap<B256, B256> = account
        .storage
        .iter()
        .filter(|(_, value)| **value != U256::ZERO)
        .map(|(slot, value)| (B256::from(*slot), B256::from(*value)))
        .collect();
    let untouched =
        info.nonce == 0 && info.balance.is_zero() && code.is_none() && storage.is_empty();
    if untouched {
        return Ok(None);
    }
    Ok(Some(GenesisAccount {
        nonce: (info.nonce != 0).then_some(info.nonce),
        balance: info.balance,
        code,
        storage: (!storage.is_empty()).then_some(storage),
        private_key: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, PageParams, Query};
    use arkiv_interfaces::statemanager::ReadMode;
    use arkiv_reth_mpt_committed_store::entities::layout::{
        SYSTEM_ACCOUNT_ADDRESS, entity_leaf_address, nonce_slot,
    };
    use arkiv_reth_mpt_committed_store::{
        AccountCode, CodeBackend, IndexStorage, RethEntityCreationNoncesStore, RethEntityStore,
        all_entities_bucket, decode,
    };
    use core::convert::Infallible;

    /// A read-only view over an alloc, implementing the store seams so the
    /// seeded state can be queried exactly as a node would query it.
    struct AllocState<'a>(&'a BTreeMap<Address, GenesisAccount>);

    impl AccountCode for AllocState<'_> {
        type Error = Infallible;
        fn code(&mut self, addr: Address) -> Result<Vec<u8>, Infallible> {
            Ok(self
                .0
                .get(&addr)
                .and_then(|a| a.code.as_ref())
                .map(|c| c.to_vec())
                .unwrap_or_default())
        }
        fn set_code(&mut self, _: Address, _: Vec<u8>) -> Result<(), Infallible> {
            unreachable!("read-only")
        }
        fn clear_code(&mut self, _: Address) -> Result<(), Infallible> {
            unreachable!("read-only")
        }
    }

    impl IndexStorage for AllocState<'_> {
        type Error = Infallible;
        fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Infallible> {
            Ok(self
                .0
                .get(&addr)
                .and_then(|a| a.storage.as_ref())
                .and_then(|s| s.get(&slot))
                .copied()
                .unwrap_or(B256::ZERO))
        }
        fn set_storage(&mut self, _: Address, _: B256, _: B256) -> Result<(), Infallible> {
            unreachable!("read-only")
        }
        fn ensure_account_persists(&mut self, _: Address) -> Result<(), Infallible> {
            unreachable!("read-only")
        }
    }

    fn owners(n: u8) -> Vec<Address> {
        (1..=n).map(|b| Address::repeat_byte(b)).collect()
    }

    fn small_spec() -> SeedSpec {
        let mut spec = SeedSpec::new(1337, owners(2));
        spec.count = 25;
        spec.payload_size = 64;
        spec.batch_size = 10; // several batches, one of them partial
        spec.expires_at = 500;
        spec
    }

    fn keys_matching(alloc: &BTreeMap<Address, GenesisAccount>, query: &Query) -> Vec<B256> {
        let mut index = RethAuxStore::new(AllocState(alloc));
        let matches = index
            .evaluate(
                query,
                PageParams {
                    page_size: 1000,
                    cursor: None,
                },
            )
            .unwrap();
        matches.keys.into_iter().map(B256::from).collect()
    }

    #[test]
    fn seeded_state_reads_back_through_the_store() {
        let spec = small_spec();
        let mut reports = Vec::new();
        let state = build_in_memory(&spec, BTreeMap::new(), |p| reports.push(p)).unwrap();
        assert_eq!(reports.len(), 3);
        assert_eq!(reports.last().unwrap().done, 25);
        assert_eq!(state.manifest.count, 25);
        assert_eq!(state.manifest.sample_keys.len(), 16);
        assert_eq!(state.manifest.accounts, state.alloc.len() as u64);

        // Every entity is at its leaf with the env-resolved fields.
        for i in 0..spec.count {
            let Op::Create { key, .. } = spec.create_op(i).unwrap() else {
                panic!()
            };
            let account = &state.alloc[&entity_leaf_address(key)];
            let entity = decode(account.code.as_ref().unwrap()).unwrap();
            assert_eq!(entity.key, key);
            assert_eq!(entity.owner, spec.owner_of(i).into_array());
            assert_eq!(entity.creator, entity.owner);
            assert_eq!(entity.created_at_block, 0);
            assert_eq!(entity.expires_at, 500);
            assert_eq!(entity.payload, spec.payload_for(i));
            assert_eq!(entity.content_type, b"application/octet-stream");
            assert_eq!(entity.attributes.len(), 2);
            assert_eq!(account.nonce, Some(1), "EIP-161 keep-alive");
        }

        // The store reads it the same way.
        let mut store = RethEntityStore::new(CodeBackend::new(AllocState(&state.alloc)));
        let key0 = state.manifest.sample_keys[0].0;
        assert!(store.get(key0).unwrap().is_some());
        assert!(store.get([0xEE; 32]).unwrap().is_none());

        // The index answers every class of query over the seed.
        let all = keys_matching(&state.alloc, &Query::All);
        assert_eq!(all.len(), 25);
        let owner0 = keys_matching(
            &state.alloc,
            &Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Owner),
                value: AnnotVal::EthereumAddress(spec.owners[0].into_array()),
            },
        );
        assert_eq!(owner0.len(), 13);
        let rank7 = keys_matching(
            &state.alloc,
            &Query::Eq {
                key: AnnotKey::User("rank".into()),
                value: AnnotVal::u256_from_u64(7),
            },
        );
        assert_eq!(rank7.len(), 1);
        let rank_ge_20 = keys_matching(
            &state.alloc,
            &Query::Gte {
                key: AnnotKey::User("rank".into()),
                value: AnnotVal::u256_from_u64(20),
            },
        );
        assert_eq!(rank_ge_20.len(), 5);
        let red = keys_matching(
            &state.alloc,
            &Query::StartsWith {
                key: AnnotKey::User("team".into()),
                value: AnnotVal::Str("re".into()),
            },
        );
        assert_eq!(red.len(), 9);
        let live = keys_matching(
            &state.alloc,
            &Query::Gt {
                key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
                value: AnnotVal::u256_from_u64(499),
            },
        );
        assert_eq!(live.len(), 25);
        let dead = keys_matching(
            &state.alloc,
            &Query::Gt {
                key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
                value: AnnotVal::u256_from_u64(500),
            },
        );
        assert!(dead.is_empty());

        // Minting nonces landed on the system account, per owner.
        let mut nonces = RethEntityCreationNoncesStore::new(AllocState(&state.alloc));
        assert_eq!(
            nonces
                .get_entity_nonce(spec.owners[0].into_array())
                .unwrap()
                .get(),
            13
        );
        assert_eq!(
            nonces
                .get_entity_nonce(spec.owners[1].into_array())
                .unwrap()
                .get(),
            12
        );
        assert_eq!(state.manifest.owner_nonces[&spec.owners[0]], 13);
        let system = &state.alloc[&SYSTEM_ACCOUNT_ADDRESS];
        assert_eq!(system.nonce, Some(1), "the system account survives EIP-161");
        assert!(
            system
                .storage
                .as_ref()
                .unwrap()
                .contains_key(&nonce_slot(spec.owners[0]))
        );
        assert!(state.alloc.contains_key(&all_entities_bucket()));
    }

    #[test]
    fn a_seed_is_deterministic() {
        let spec = small_spec();
        let a = build_in_memory(&spec, BTreeMap::new(), |_| {}).unwrap();
        let b = build_in_memory(&spec, BTreeMap::new(), |_| {}).unwrap();
        assert_eq!(a.alloc, b.alloc);
        assert_eq!(a.manifest.state_root, b.manifest.state_root);

        let mut other = spec;
        other.seed = 1;
        let c = build_in_memory(&other, BTreeMap::new(), |_| {}).unwrap();
        assert_ne!(a.manifest.state_root, c.manifest.state_root);
    }

    /// Batch size must not change the state: the same entities in different
    /// batches land byte-identical accounts, because ids follow seed order.
    /// (Holds while no range-index node splits — the seed here has at most 25
    /// distinct values per attribute, well under a node's 32 keys.)
    #[test]
    fn batching_does_not_change_the_state() {
        let one = build_in_memory(&small_spec(), BTreeMap::new(), |_| {}).unwrap();
        let mut spec = small_spec();
        spec.batch_size = 1000;
        let all_at_once = build_in_memory(&spec, BTreeMap::new(), |_| {}).unwrap();
        assert_eq!(one.manifest.state_root, all_at_once.manifest.state_root);
        assert_eq!(one.alloc, all_at_once.alloc);

        // Entity `i` holds index id `i`.
        let mut index = RethAuxStore::new(AllocState(&one.alloc));
        let all: Vec<u64> = index.all_entities().unwrap().iter().collect();
        assert_eq!(all, (0..25).collect::<Vec<_>>());
        for i in 0..spec.count {
            let Op::Create { key, .. } = spec.create_op(i).unwrap() else {
                panic!()
            };
            assert_eq!(index.key_of_id(i).unwrap(), Some(key), "entity {i}");
        }
    }

    /// What stays in the builder's cache between batches is the seed's shape:
    /// the same attribute values over four times the entities leave the same
    /// accounts cached, and no slots for the entities themselves.
    #[test]
    fn the_cache_holds_the_shape_not_the_size() {
        fn last_progress(count: u64) -> Progress {
            let mut spec = small_spec();
            spec.count = count;
            spec.batch_size = 50;
            let mut last = None;
            build_in_memory(&spec, BTreeMap::new(), |p| last = Some(p)).unwrap();
            last.unwrap()
        }
        // 100 entities cover every rank (mod 100) and team; 400 add nothing.
        let small = last_progress(100);
        let large = last_progress(400);
        assert_eq!(large.cached_accounts, small.cached_accounts);
        assert_eq!(large.cached_slots, small.cached_slots);
        assert!(small.cached_accounts < 200, "{}", small.cached_accounts);
    }

    /// The streamed dump is the in-memory alloc, account for account, with
    /// the same root — base funding included — and refuses a funded address
    /// the seed also writes.
    #[test]
    fn streaming_matches_memory() {
        use crate::sink::test_support::read_dump;
        let spec = small_spec();
        let funded = Address::repeat_byte(0xF0);
        let base: BTreeMap<Address, GenesisAccount> = [(
            funded,
            GenesisAccount {
                balance: U256::from(10u64).pow(U256::from(20u64)),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let memory = build_in_memory(&spec, base.clone(), |_| {}).unwrap();
        assert!(memory.alloc.contains_key(&funded));

        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("state.jsonl");
        let mut sink = crate::sink::StreamSink::create(&dump, dir.path().join("sort")).unwrap();
        let manifest = build(&spec, base, &mut sink, |_| {}).unwrap();
        assert_eq!(manifest, memory.manifest);
        let (root, alloc) = read_dump(&dump);
        assert_eq!(root, manifest.state_root);
        assert_eq!(alloc, memory.alloc);
        assert_eq!(alloc.len() as u64, manifest.accounts);

        let clash: BTreeMap<Address, GenesisAccount> =
            [(SYSTEM_ACCOUNT_ADDRESS, GenesisAccount::default())]
                .into_iter()
                .collect();
        assert!(build_in_memory(&spec, clash, |_| {}).is_err());
    }

    /// Nonce continuity: a create after genesis mints the next key.
    #[test]
    fn the_next_create_continues_the_nonce_sequence() {
        let spec = small_spec();
        let state = build_in_memory(&spec, BTreeMap::new(), |_| {}).unwrap();
        let owner = spec.owners[0];
        let next = state.manifest.owner_nonces[&owner];
        let next_key = arkiv_reth_executor::derive_entity_address(
            spec.chain_id,
            &owner.into_array(),
            arkiv_interfaces::primitives::EntityCreationNonce::new(next),
            0,
        );
        // Not seeded yet...
        let mut store = RethEntityStore::new(CodeBackend::new(AllocState(&state.alloc)));
        assert!(store.get(next_key).unwrap().is_none());
        // ...but every earlier nonce of this owner is.
        for n in 0..next {
            let key = arkiv_reth_executor::derive_entity_address(
                spec.chain_id,
                &owner.into_array(),
                arkiv_interfaces::primitives::EntityCreationNonce::new(n),
                0,
            );
            assert!(store.get(key).unwrap().is_some(), "nonce {n}");
        }
        let _ = ReadMode::ViewOnBase;
        let _ = AttributeValue::Bool(true);
    }
}
