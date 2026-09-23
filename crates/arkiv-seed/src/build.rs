//! Build genesis through the live authenticated state manager.
use crate::{
    manifest::SeedManifest,
    sink::{AccountSink, MemorySink},
    spec::SeedSpec,
};
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_authenticated_store::{State, Store};
use arkiv_interfaces::execution::{ExecEnv, ExecStatus};
use arkiv_interfaces::statemanager::{BlockRef, EntityCreationNoncesStore, StateView};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_reth_statemanager::{
    authenticated::{ROOT_ACCOUNT, root_slot},
    chain::chain_manager,
};
use eyre::{Result, WrapErr, bail};
use reth_ethereum::evm::revm::{
    DatabaseCommit,
    database_interface::EmptyDB,
    db::{AccountState, CacheDB, DbAccount},
    primitives::KECCAK_EMPTY,
};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct SeededState {
    /// Native funding/predeploys and the single authenticated root account.
    pub alloc: BTreeMap<Address, GenesisAccount>,
    pub manifest: SeedManifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub elapsed_ms: u128,
    pub cached_accounts: usize,
    pub cached_slots: usize,
}

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

/// Entity IDs follow seed order, independent of batching or owner grouping.
/// The resulting manifest contains the portable authenticated snapshot; exporters
/// must include it in config.arkivState alongside the native alloc/stateHash.
pub fn build(
    spec: &SeedSpec,
    base: BTreeMap<Address, GenesisAccount>,
    sink: &mut impl AccountSink,
    mut on_progress: impl FnMut(Progress),
) -> Result<SeedManifest> {
    spec.validate()?;
    let started = Instant::now();
    let mut db = CacheDB::new(EmptyDB::default());
    let records = Store::default();
    let mut sample_keys = Vec::new();
    let mut next = 0;
    while next < spec.count {
        let end = (next + spec.batch_size as u64).min(spec.count);
        let mut view = chain_manager(&mut db, BlockRef::new(0, [0; 32]), records.clone())?;
        let executor = ArkivExecutor::new();
        for i in next..end {
            let op = spec.create_op(i)?;
            if sample_keys.len() < SeedManifest::SAMPLE_KEYS {
                sample_keys.push(B256::from(*op.key()));
            }
            let env = ExecEnv {
                caller: spec.owner_of(i).into_array(),
                block_number: 0,
                gas_supplied: u64::MAX,
                chain_id: spec.chain_id,
            };
            let out = executor
                .apply(&env, &mut view, &[op])
                .map_err(|e| eyre::eyre!("seed entity {i}: {e}"))?;
            if out.status != ExecStatus::Ok {
                bail!("seed entity {i} reverted: {:?}", out.revert);
            }
            view.fetch_increment_entity_creation_nonce(env.caller)?;
        }
        StateView::commit(&mut view).map_err(|e| eyre::eyre!("commit seed: {e:?}"))?;
        let diff = view.into_base().into_state();
        db.commit(diff);
        next = end;
        on_progress(Progress {
            done: next,
            total: spec.count,
            elapsed_ms: started.elapsed().as_millis(),
            cached_accounts: db.cache.accounts.len(),
            cached_slots: db.cache.accounts.values().map(|a| a.storage.len()).sum(),
        });
    }
    let root = db
        .cache
        .accounts
        .get(&ROOT_ACCOUNT)
        .and_then(|a| a.storage.get(&root_slot()))
        .copied()
        .unwrap_or_default();
    let authenticated_state = State::open(records, B256::from(root))?.snapshot()?;
    for (address, account) in &db.cache.accounts {
        if let Some(genesis) = genesis_account(*address, account, &db)? {
            sink.account(*address, genesis)?;
        }
    }
    for (address, account) in base {
        sink.account(address, account)
            .wrap_err("base alloc conflicts with the system account")?;
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
        authenticated_state,
    })
}

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
    use arkiv_interfaces::query::{AnnotKey, AnnotVal, PageParams, Query};
    fn spec() -> SeedSpec {
        let mut s = SeedSpec::new(1337, vec![Address::repeat_byte(1), Address::repeat_byte(2)]);
        s.count = 25;
        s.batch_size = 10;
        s.expires_at = 500;
        s.payload_size = 64;
        s
    }
    #[test]
    fn portable_genesis_roundtrip_and_bounded_queries() {
        let s = spec();
        let built = build_in_memory(&s, BTreeMap::new(), |_| {}).unwrap();
        assert_eq!(built.alloc.len(), 1);
        assert!(built.alloc.contains_key(&ROOT_ACCOUNT));
        let records = Store::default();
        built
            .manifest
            .authenticated_state
            .import(records.clone())
            .unwrap();
        let state = State::open(records, built.manifest.authenticated_state.root).unwrap();
        for i in 0..s.count {
            let key = *s.create_op(i).unwrap().key();
            let entity = state.entity(key).unwrap().unwrap();
            assert_eq!(entity.payload, s.payload_for(i));
            assert_eq!(entity.owner, s.owner_of(i).into_array());
            assert_eq!(entity.expires_at, 500);
        }
        for (owner, nonce) in &built.manifest.owner_nonces {
            assert_eq!(state.creation_nonce(owner.into_array()).unwrap(), *nonce);
        }
        let matches = state
            .page(
                &Query::Gte {
                    key: AnnotKey::User("rank".into()),
                    value: AnnotVal::u256_from_u64(20),
                },
                PageParams {
                    page_size: 100,
                    cursor: None,
                },
            )
            .unwrap();
        assert_eq!(matches.keys.len(), 5);
        assert_eq!(state.expired(499, 100, u64::MAX).unwrap().len(), 0);
        assert_eq!(state.expired(500, 100, u64::MAX).unwrap().len(), 25);
        assert_eq!(state.expired(500, 3, u64::MAX).unwrap().len(), 3);
    }
    #[test]
    fn batch_independent_commitment() {
        let a = build_in_memory(&spec(), BTreeMap::new(), |_| {}).unwrap();
        let mut s = spec();
        s.batch_size = 100;
        let b = build_in_memory(&s, BTreeMap::new(), |_| {}).unwrap();
        assert_eq!(a.alloc, b.alloc);
        assert_eq!(a.manifest, b.manifest);
        s.seed = 1;
        let c = build_in_memory(&s, BTreeMap::new(), |_| {}).unwrap();
        assert_ne!(a.manifest.state_root, c.manifest.state_root);
    }
    #[test]
    fn streaming_and_memory_match_and_reject_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("state.jsonl");
        let mut sink = crate::StreamSink::create(&dump, dir.path().join("sort")).unwrap();
        let m = build(&spec(), BTreeMap::new(), &mut sink, |_| {}).unwrap();
        let a = build_in_memory(&spec(), BTreeMap::new(), |_| {}).unwrap();
        assert_eq!(a.manifest, m);
        let (root, alloc) = crate::sink::test_support::read_dump(&dump);
        assert_eq!(root, m.state_root);
        assert_eq!(alloc, a.alloc);
        assert!(
            build_in_memory(
                &spec(),
                [(ROOT_ACCOUNT, GenesisAccount::default())].into(),
                |_| {}
            )
            .is_err()
        );
    }
}
