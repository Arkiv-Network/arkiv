//! Run with: cargo run -p arkiv-reth-statemanager --example authenticated_state -- /tmp/arkiv-tree 1000
use alloy_primitives::{Address, B256, U256};
use arkiv_authenticated_store::{MdbxStore, State};
use arkiv_interfaces::entity::{Attribute, AttributeType, AttributeValue, Entity};
use arkiv_reth_mpt_committed_store::{BalanceAccess, NonceAccess};
use arkiv_reth_statemanager::authenticated::{AuthenticatedOverlay, ROOT_ACCOUNT, root_slot};
use reth_ethereum::evm::revm::{
    database_interface::{DatabaseCommit, EmptyDB},
    db::CacheDB,
    state::AccountInfo,
};
use std::{ops::Bound, time::Instant};

fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| eyre::eyre!("usage: authenticated_state <mdbx-directory> [entity-count]"))?;
    let count = args
        .next()
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(1000);
    eyre::ensure!(count >= 40, "use at least 40 entities");
    let store = MdbxStore::open(&path)?;
    let alice = Address::repeat_byte(1);
    let mut ethereum = CacheDB::new(EmptyDB::default());
    ethereum.insert_account_info(
        alice,
        AccountInfo {
            balance: U256::from(1000),
            ..Default::default()
        },
    );
    let start = Instant::now();
    let (root, diff) =
        AuthenticatedOverlay::open(&mut ethereum, store.clone())?.execute(|state, native| {
            for age in 0..count {
                let entity = Entity {
                    key: B256::from(U256::from(age + 1)).0,
                    creator: alice.into_array(),
                    owner: alice.into_array(),
                    payload: format!("entity {age}").into_bytes(),
                    content_type: b"text/plain".to_vec(),
                    expires_at: 1_000_000,
                    attributes: vec![Attribute::new(b"age".to_vec(), AttributeValue::U64(age))],
                    ..Default::default()
                };
                state.put_entity(&entity)?;
                state.increment_creation_nonce(alice.into_array())?;
            }
            // Illustrative fee/nonce effects; real fee policy belongs to the executor.
            native.set_balance(alice, U256::from(990))?;
            native.set_nonce(alice, 1)?;
            Ok(state.root())
        })?;
    println!(
        "Built and persisted {count} entities in {:?}",
        start.elapsed()
    );
    println!("Arkiv root: {root}");
    println!(
        "Ethereum diff: {} accounts (sender + root account), {} root storage slot",
        diff.len(),
        diff[&ROOT_ACCOUNT].storage.len()
    );
    assert_eq!(diff.len(), 2);
    assert_eq!(
        diff[&ROOT_ACCOUNT].storage[&root_slot()].present_value,
        U256::from_be_bytes(root.0)
    );
    ethereum.commit(diff);
    drop(store);

    // Reopen the actual MDBX environment, using the committed root from Ethereum.
    let reopened = MdbxStore::open(&path)?;
    let ((), no_changes) =
        AuthenticatedOverlay::open(&mut ethereum, reopened.clone())?.execute(|state, native| {
            let start = Instant::now();
            let (hits, stats) = state.range(
                b"age",
                AttributeType::U64,
                Bound::Included(&AttributeValue::U64(30)),
                Bound::Excluded(&AttributeValue::U64(40)),
            )?;
            println!(
                "30 <= age < 40: {} entities, {} tree nodes and {} bitmap records read, {:?}",
                hits.len(),
                stats.nodes_read,
                stats.values_read,
                start.elapsed()
            );
            assert_eq!(hits.len(), 10);
            assert_eq!(stats.values_read, 10);
            println!(
                "Native balance: {}; tx nonce: {}; entity-creation nonce: {}",
                native.get_balance(alice)?,
                native.get_nonce(alice)?,
                state.creation_nonce(alice.into_array())?
            );
            Ok(())
        })?;
    assert!(no_changes.is_empty());
    let mut fork = State::open(reopened.clone(), root)?;
    fork.remove_entity(B256::from(U256::from(31)).0)?;
    let fork_root = fork.persist()?;
    assert_ne!(root, fork_root);
    assert!(
        State::open(reopened, root)?
            .entity(B256::from(U256::from(31)).0)?
            .is_some()
    );
    println!("Fork root: {fork_root}; original snapshot still contains the deleted entity");
    println!("Ethereum state in this demo is in memory; only the custom records persist in {path}");
    Ok(())
}
