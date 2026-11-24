//! Writing one archive into the store, as one commit, through the executor.
//!
//! Every line runs through [`ArkivExecutor`] as a transaction at block 0
//! would: the same business rules (reserved and duplicate keys, attribute
//! limits, the permanent-expiry check) and GolemDB's cell limits, so bad data
//! is refused the way a bad transaction is. Two things differ, as decided in
//! #338:
//!
//! - **No lifetime cap.** Each entity keeps the expiry its line gives:
//!   `max_lifetime` limits transactions, not prefilled data.
//! - **Free.** Charging the sender and crediting the storage pool live in the
//!   node's transaction wrapper, which a prefill never goes through. The
//!   executor still computes gas; it is ignored.
//!
//! The commit is tagged with the archive's hash, computed from the very bytes
//! the entities were read from ([`Entities::hash`]), and counts one more
//! archive in the prefill record ([`prefill::bump`]).
//!
//! [`Entities::hash`]: crate::Entities::hash
//! [`prefill::bump`]: arkiv_golemdb_state::prefill::bump

use alloy_primitives::B256;
use arkiv_golemdb_state::{GolemStateView, prefill};
use arkiv_interfaces::execution::{ExecEnv, ExecStatus, RevertReason};
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::statemanager::{BlockRef, StateView};
use arkiv_interfaces::store::{BranchId, CommitId, StoreExt};
use arkiv_reth_executor::ArkivExecutor;
use arkiv_tokenomics::FeeSchedule;
use eyre::{Result, eyre};

use crate::Archive;

/// What writing an archive produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The commit holding the archive.
    pub commit: CommitId,
    /// The archive's hash, which tags that commit.
    pub hash: B256,
    /// How many entities it wrote.
    pub entities: u64,
}

/// Write `archive` into `store` as one commit on top of its head, for the
/// chain `chain_id` (entity keys and the executor's rules may depend on it).
///
/// The store must hold a started prefill record ([`prefill::start`]), which
/// this counts one more archive in. On any error, nothing is committed: a
/// refused line, with its `file:line` and the executor's reason, leaves the
/// store as it was.
///
/// [`prefill::start`]: arkiv_golemdb_state::prefill::start
pub fn write_archive<S: StoreExt + Clone>(
    store: &S,
    archive: &Archive,
    chain_id: u64,
) -> Result<Written> {
    let origin = store.head();
    let pricing = store
        .pricing_snapshot(origin)
        .map_err(|e| eyre!("read the store's fee schedule: {e:?}"))?;
    let schedule = FeeSchedule::from_model(&pricing.model)
        .map_err(|e| eyre!("the store's fee schedule: {e:?}"))?;
    let branch = store
        .begin(None)
        .map_err(|e| eyre!("open a branch for {}: {e:?}", archive.index().display()))?;
    let written =
        stage(store, archive, chain_id, schedule, branch, origin).and_then(|(hash, entities)| {
            let commit = store
                .commit_tagged(branch, hash.0)
                .map_err(|e| eyre!("commit {}: {e:?}", archive.index().display()))?;
            Ok(Written {
                commit,
                hash,
                entities,
            })
        });
    if written.is_err() {
        // Leave nothing half-written behind, and free what the branch holds,
        // a failed commit included.
        let _ = store.discard(branch);
    }
    written
}

/// Every line of `archive` on `branch`, then the prefill count; the archive
/// hash and how many entities there were.
fn stage<S: StoreExt + Clone>(
    store: &S,
    archive: &Archive,
    chain_id: u64,
    schedule: FeeSchedule,
    branch: BranchId,
    origin: CommitId,
) -> Result<(B256, u64)> {
    let executor = ArkivExecutor::new(schedule);
    let mut lines = archive.entities();
    let mut entities = 0u64;
    while let Some(line) = lines.next() {
        // A malformed line already names its place.
        let (creator, ops) = line?;
        // Formatted only for an error.
        let at = || lines.at().unwrap_or_default();
        let env = ExecEnv {
            caller: creator,
            block_number: 0,
            gas_supplied: u64::MAX,
            tx_gas: 0,
            floor_gas: 0,
            golemdb_cost_before: 0,
            chain_id,
            max_lifetime: BlockNumber::MAX,
            keep_writes: true,
        };
        // One view per line, as the node opens one per transaction. Every view
        // stages onto the same branch, so a line sees the lines before it.
        let mut view =
            GolemStateView::new(store.clone(), branch, origin, BlockRef::new(0, [0; 32]));
        let out = executor
            .apply(&env, &mut view, &ops)
            .map_err(|e| eyre!("{}: the executor failed: {e:?}", at()))?;
        if out.status == ExecStatus::Reverted {
            let reason = match out.revert {
                // With all the gas there is, only a cost past 64 bits runs out.
                Some(RevertReason::OutOfGas) => {
                    "OutOfGas: its cost does not fit 64 bits; is its expiry that far out?"
                        .to_owned()
                }
                Some(r) => format!("{r:?}"),
                None => "no reason given".to_owned(),
            };
            return Err(eyre!("{}: refused: {reason}", at()));
        }
        StateView::commit(&mut view).map_err(|e| eyre!("{}: stage the entity: {e:?}", at()))?;
        entities += 1;
    }
    let hash = lines.hash().ok_or_else(|| {
        eyre!(
            "{}: the archive was not read to its end",
            archive.index().display()
        )
    })?;
    prefill::bump(store, branch).map_err(|e| eyre!("count the archive: {e:?}"))?;
    Ok((hash, entities))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use arkiv_golemdb_state::accounts::record_key;
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::primitives::UserBalance;
    use arkiv_interfaces::statemanager::{
        AccountBalancesStore, EntityCreationNoncesStore, EntityStore, ReadMode,
    };
    use arkiv_interfaces::store::{ReadTarget, Store};
    use arkiv_reth_statemanager::{GenesisAccount, HostStore, seed_genesis};

    use crate::testkit::{ALICE, BOB, CHAIN, line, store};

    /// A store with a funded Alice, as the alloc commit leaves it, and a
    /// started prefill of `total` archives.
    fn started(total: u64) -> HostStore {
        let store = store();
        // The alloc commit, written here as genesis would: Alice with 100.
        seed_genesis(
            &store,
            [0xa1; 32],
            [GenesisAccount {
                address: ALICE.parse().unwrap(),
                balance: U256::from(100),
                nonce: 0,
            }],
        )
        .unwrap();
        let branch = store.begin(None).unwrap();
        prefill::start(&*store, branch, total, [0x4f; 32]).unwrap();
        store.commit(branch).unwrap();
        store
    }

    /// An archive of one part holding `lines`.
    fn archive(lines: &[String]) -> (tempfile::TempDir, Archive) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.jsonl"), lines.join("\n") + "\n").unwrap();
        let index = dir.path().join("index.json");
        std::fs::write(&index, r#"{"version":1,"parts":["p.jsonl"]}"#).unwrap();
        (dir, Archive::open(index).unwrap())
    }

    fn view(store: &HostStore, at: CommitId) -> GolemStateView<HostStore> {
        GolemStateView::new(
            store.clone(),
            store.begin(Some(at)).unwrap(),
            at,
            BlockRef::new(1, [0; 32]),
        )
    }

    /// The entities land at block 0, an owner other than the creator gets the
    /// entity, the commit carries the archive's hash and counts the archive.
    /// An expiry far past `max_lifetime` (7,884,000 here) stands.
    #[test]
    fn an_archive_becomes_one_tagged_commit() {
        let store = started(2);
        let far = 1_000_000_000;
        let (_dir, archive) = archive(&[
            line(1, ALICE, ALICE, 1_500_000, r#","kind":"str:a""#),
            line(2, ALICE, BOB, far, ""),
        ]);
        let written = write_archive(&store, &archive, CHAIN).unwrap();
        assert_eq!(written.entities, 2);
        assert_eq!(written.hash, archive.hash().unwrap());
        assert_eq!(store.head(), written.commit);
        assert_eq!(store.tag_of(written.commit).unwrap(), Some(written.hash.0));
        assert_eq!(
            prefill::read(&*store, ReadTarget::Commit(written.commit))
                .unwrap()
                .map(|c| c.commits),
            Some(1)
        );

        let view = view(&store, written.commit);
        let first = view
            .get_entity([1; 32], ReadMode::ViewOnBase)
            .unwrap()
            .value
            .unwrap();
        assert_eq!((first.created_at, first.updated_at), (0, 0));
        assert_eq!(first.expires_at, 1_500_000);
        assert_eq!(first.attributes[0].value, AttributeValue::Str("a".into()));
        let second = view
            .get_entity([2; 32], ReadMode::ViewOnBase)
            .unwrap()
            .value
            .unwrap();
        assert_eq!(second.expires_at, far, "no max_lifetime cap");
        assert_eq!(
            second.creator,
            ALICE.parse::<alloy_primitives::Address>().unwrap().0.0
        );
        assert_eq!(
            second.owner,
            BOB.parse::<alloy_primitives::Address>().unwrap().0.0
        );
        // Each create advances its creator's minting nonce, as a transaction's
        // would, though the archive gave the keys: Alice's next derived key
        // comes from nonce 2. Bob, only an owner, created nothing.
        let nonce = |who: [u8; 20]| {
            view.get_entity_creation_nonce(who, ReadMode::ViewOnBase)
                .unwrap()
                .value
                .get()
        };
        assert_eq!((nonce([0x11; 20]), nonce([0x22; 20])), (2, 0));
    }

    /// Prefill is free: no balance moves, the storage pool's included.
    #[test]
    fn nobody_pays() {
        let store = started(1);
        let (_dir, archive) = archive(&[line(1, ALICE, ALICE, 1_500_000, "")]);
        let written = write_archive(&store, &archive, CHAIN).unwrap();
        let view = view(&store, written.commit);
        let balance = |who: [u8; 20]| view.get_balance(who, ReadMode::ViewOnBase).unwrap();
        assert_eq!(balance([0x11; 20]), UserBalance::from_u64(100), "Alice");
        let pool = arkiv_reth_executor::ARKIV_ADDRESS.0.0;
        assert_eq!(balance(pool), UserBalance::from_u64(0), "the storage pool");
    }

    /// A line the executor refuses stops the run with its place and the
    /// reason, and nothing is committed: not the lines before it, not the
    /// count.
    #[test]
    fn a_refused_line_commits_nothing() {
        let store = started(1);
        let head = store.head();
        // Line 2 reuses line 1's key.
        let (_dir, archive) = archive(&[
            line(1, ALICE, ALICE, 1_500_000, ""),
            line(1, ALICE, ALICE, 1_500_000, ""),
        ]);
        let error = write_archive(&store, &archive, CHAIN)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("p.jsonl:2: refused: AlreadyExists"),
            "{error}"
        );
        assert_eq!(store.head(), head, "nothing committed");
        assert_eq!(
            prefill::read(&*store, ReadTarget::Commit(head))
                .unwrap()
                .map(|c| c.commits),
            Some(0)
        );
    }

    /// A line naming a reserved key, an account's, is refused like a
    /// transaction naming one.
    #[test]
    fn a_reserved_key_is_refused() {
        let store = started(1);
        let account = B256::from(record_key([0x22; 20]).0);
        let reserved = format!(
            r#"{{"$key":"{account}","$creator":"{ALICE}","$owner":"{ALICE}","$expiresAt":5}}"#
        );
        let (_dir, archive) = archive(&[reserved]);
        let error = write_archive(&store, &archive, CHAIN)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("p.jsonl:1: refused: AlreadyExists"),
            "{error}"
        );
    }

    /// A large entity expiring just short of permanent costs more than 64 bits
    /// of gas, and says so rather than only "out of gas".
    #[test]
    fn a_cost_past_64_bits_names_the_expiry() {
        let store = started(1);
        // 96 KiB of zeros, as base64.
        let payload = format!(r#","$payload":"{}""#, "A".repeat(128 * 1024));
        let (_dir, archive) = archive(&[line(1, ALICE, ALICE, u64::MAX - 1, &payload)]);
        let error = write_archive(&store, &archive, CHAIN)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("p.jsonl:1: refused: OutOfGas: its cost does not fit 64 bits"),
            "{error}"
        );
    }

    /// Counting stops at the total, so an archive past it is refused.
    #[test]
    fn an_archive_past_the_total_is_refused() {
        let store = started(1);
        let (_dir, first) = archive(&[line(1, ALICE, ALICE, 5, "")]);
        write_archive(&store, &first, CHAIN).unwrap();
        let head = store.head();
        let (_dir, second) = archive(&[line(2, ALICE, ALICE, 5, "")]);
        let error = write_archive(&store, &second, CHAIN)
            .unwrap_err()
            .to_string();
        assert!(error.contains("AllCommitted"), "{error}");
        assert_eq!(store.head(), head);
    }
}
