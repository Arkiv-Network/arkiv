//! Materialising a [`SeedSpec`] into account state, through the node's own
//! write path.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_trie::root::state_root_ref_unhashed;
use arkiv_interfaces::entity::Entity;
use arkiv_interfaces::execution::{ExecEnv, ExecStatus, Op};
use arkiv_interfaces::statemanager::{BlockRef, EntityCreationNoncesStore, EntityStore, StateView};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_reth_mpt_committed_store::{AuxiliaryEntityDelta, RethAuxStore, annotation_delta};
use arkiv_reth_statemanager::write_manager;
use eyre::{Result, WrapErr, bail};
use reth_ethereum::evm::revm::{
    DatabaseCommit,
    database_interface::EmptyDB,
    db::{AccountState, CacheDB},
    primitives::KECCAK_EMPTY,
};

use crate::manifest::SeedManifest;
use crate::spec::SeedSpec;

/// The accounts a seed produced, plus the record of what was seeded.
#[derive(Debug, Clone)]
pub struct SeededState {
    /// Every account the seed wrote: entity records, index buckets and the
    /// system account. Base funding is merged in by [`export`](crate::export).
    pub alloc: BTreeMap<Address, GenesisAccount>,
    /// What was built. Its `state_root` and `accounts` describe this alloc
    /// alone until [`export::finish`](crate::export::finish) merges funding in.
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
}

/// Build the state `spec` describes.
///
/// Batches of `spec.batch_size` creates run through the executor into a
/// `MptStateView` over the production `WriteOverlay`, whose base is revm's
/// in-memory `CacheDB`. Each batch commits like a block would — entities as
/// account code, then the index, then the owners' minting nonces — and the
/// resulting diff is folded into the cache. `on_progress` is called after every
/// batch.
pub fn build(spec: &SeedSpec, mut on_progress: impl FnMut(Progress)) -> Result<SeededState> {
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
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("commit seed batch: {e:?}"))?;
        RethAuxStore::new(view.backend_mut())
            .apply_inserts_bulk(&deltas)
            .map_err(|e| eyre::eyre!("index seed batch: {e:?}"))?;
        let state = view.into_base().into_state();
        db.commit(state);

        next = end;
        on_progress(Progress {
            done: next,
            total: spec.count,
            elapsed_ms: started.elapsed().as_millis(),
        });
    }

    let alloc = export_cache(&db).wrap_err("export seeded accounts")?;
    let manifest = SeedManifest {
        chain_id: spec.chain_id,
        count: spec.count,
        payload_size: spec.payload_size,
        content_type: spec.content_type.clone(),
        owners: spec.owners.clone(),
        expires_at: spec.expires_at,
        attributes: spec.attributes.iter().map(ToString::to_string).collect(),
        seed: spec.seed,
        state_root: state_root_ref_unhashed(alloc.iter()),
        accounts: alloc.len() as u64,
        sample_keys,
        owner_nonces: spec.owner_nonces(),
    };
    Ok(SeededState { alloc, manifest })
}

/// Read every account the write path left in the cache back out as genesis
/// accounts: nonce, balance, code, and the non-zero storage slots.
fn export_cache(db: &CacheDB<EmptyDB>) -> Result<BTreeMap<Address, GenesisAccount>> {
    let mut alloc = BTreeMap::new();
    for (address, account) in &db.cache.accounts {
        if account.account_state == AccountState::NotExisting {
            continue;
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
            continue;
        }
        alloc.insert(
            *address,
            GenesisAccount {
                nonce: (info.nonce != 0).then_some(info.nonce),
                balance: info.balance,
                code,
                storage: (!storage.is_empty()).then_some(storage),
                private_key: None,
            },
        );
    }
    Ok(alloc)
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
        let state = build(&spec, |p| reports.push(p)).unwrap();
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
        let a = build(&spec, |_| {}).unwrap();
        let b = build(&spec, |_| {}).unwrap();
        assert_eq!(a.alloc, b.alloc);
        assert_eq!(a.manifest.state_root, b.manifest.state_root);

        let mut other = spec;
        other.seed = 1;
        let c = build(&other, |_| {}).unwrap();
        assert_ne!(a.manifest.state_root, c.manifest.state_root);
    }

    /// Batch size must not change the state: the same entities in different
    /// batches land byte-identical accounts, because ids follow seed order.
    /// (Holds while no range-index node splits — the seed here has at most 25
    /// distinct values per attribute, well under a node's 32 keys.)
    #[test]
    fn batching_does_not_change_the_state() {
        let one = build(&small_spec(), |_| {}).unwrap();
        let mut spec = small_spec();
        spec.batch_size = 1000;
        let all_at_once = build(&spec, |_| {}).unwrap();
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

    /// Nonce continuity: a create after genesis mints the next key.
    #[test]
    fn the_next_create_continues_the_nonce_sequence() {
        let spec = small_spec();
        let state = build(&spec, |_| {}).unwrap();
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
