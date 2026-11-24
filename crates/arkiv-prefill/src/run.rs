//! The whole prefill: the alloc commit, then every archive in order, resumable
//! after a crash.
//!
//! ```text
//! commit 0     GolemDB init                         (written when GolemDB opens)
//! commit 1     alloc + prefill record (total P)     ← run, on a fresh store
//! commit 2..   one archive each, tagged with its hash
//! ```
//!
//! [`run`] looks at the store first ([`classify`]). A fresh store starts with
//! the alloc commit. A store partway through a prefill resumes at the first
//! archive not yet committed, after three checks that nothing changed since
//! the prefill began:
//!
//! - the genesis file still hashes to the recorded `$prefillGenesisHash`;
//! - the number of archives is still the recorded `$prefillTotal`;
//! - the last committed archive's tag is still the hash of the archive in that
//!   place of the list. That catches a list reordered or edited at that place,
//!   not before it: checking every earlier archive would read them all again.
//!   An earlier archive moved later is still refused, by its keys already
//!   existing; an earlier one edited in place goes unnoticed here, and shows
//!   up as a root an audit run does not reproduce.
//!
//! The archive set and the genesis file are fixed by the first commit. Any
//! change means starting again in an empty directory.
//!
//! [`classify`]: arkiv_reth_statemanager::classify

use alloy_primitives::B256;
use arkiv_golemdb_state::prefill;
use arkiv_interfaces::store::{ReadTarget, Store, StoreExt};
use arkiv_reth_statemanager::{GenesisAccount, HostStore, StoreState, classify, write_alloc};
use eyre::{Result, bail, ensure, eyre};

use crate::{Archive, Written, write_archive};

/// A block 0 hash no chain has. A prefill runs before its chain's block 0
/// exists, so no commit can be this chain's block 0 yet: one that is tagged as
/// a block 0 means the store's prefill was already finalized.
const NO_BLOCK_ZERO: [u8; 32] = [0; 32];

/// What a prefill writes.
#[derive(Debug, Clone, Copy)]
pub struct Prefill<'a> {
    /// The chain's id, from its genesis file.
    pub chain_id: u64,
    /// The genesis file's prefill hash: `ArkivChainSpec::prefill_genesis_hash`.
    pub genesis_hash: [u8; 32],
    /// The genesis file's accounts, in address order.
    pub alloc: &'a [GenesisAccount],
    /// The archives, in order.
    pub archives: &'a [Archive],
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// How many archives were already committed when it started: 0 on a fresh
    /// store.
    pub resumed_from: u64,
    /// The archives it wrote, in order.
    pub written: Vec<Written>,
}

/// Run `prefill` into `store`: start it on a fresh store, resume it on one
/// partway through, and refuse any other. `on_written` hears of each archive
/// as it is committed, with its place in the list.
///
/// When it returns, every archive is in and the store waits for block 0 to be
/// finalized. On an error, every archive committed before it stays committed,
/// and running again resumes after it.
pub fn run(
    store: &HostStore,
    prefill: &Prefill,
    mut on_written: impl FnMut(usize, &Written),
) -> Result<Ran> {
    let total = prefill.archives.len() as u64;
    let resumed_from = match classify(store, NO_BLOCK_ZERO)? {
        StoreState::GolemDbInitialized => {
            start(store, prefill, total)?;
            0
        }
        StoreState::ArkivPrefilling { done, .. } => {
            check(store, prefill, done)?;
            done
        }
        StoreState::ArkivPrefillDone => {
            check(store, prefill, total)?;
            total
        }
        StoreState::ArkivInitialized | StoreState::ArkivPrefilledOtherBlockZero => bail!(
            "the store holds a prefill whose block 0 is already finalized: there is nothing \
             left to write"
        ),
        StoreState::ArkivPrefillInconsistent { done, total, head } => bail!(
            "the store's prefill record does not fit its commits: it counts {done} of {total} \
             archives, but the head is commit {head}. The store is damaged; prefill again into \
             an empty directory"
        ),
        StoreState::ForeignChain => {
            bail!("the store holds another chain, not a prefill. Prefill into an empty directory")
        }
    };
    let mut written = Vec::new();
    for (place, archive) in prefill
        .archives
        .iter()
        .enumerate()
        .skip(resumed_from as usize)
    {
        let archive_written = write_archive(store, archive, prefill.chain_id)?;
        on_written(place, &archive_written);
        written.push(archive_written);
    }
    Ok(Ran {
        resumed_from,
        written,
    })
}

/// The alloc commit: the genesis file's accounts and the prefill record, for
/// `total` archives, as commit 1.
fn start(store: &HostStore, prefill: &Prefill, total: u64) -> Result<()> {
    let branch = store
        .begin(None)
        .map_err(|e| eyre!("open the alloc commit: {e:?}"))?;
    let written = write_alloc(store, branch, prefill.alloc.iter().copied())
        .map_err(|e| eyre!("write the genesis accounts: {e:?}"))
        .and_then(|()| {
            prefill::start(&**store, branch, total, prefill.genesis_hash)
                .map_err(|e| eyre!("write the prefill record: {e:?}"))
        });
    if let Err(error) = written {
        let _ = store.discard(branch);
        return Err(error);
    }
    store
        .commit(branch)
        .map_err(|e| eyre!("commit the alloc commit: {e:?}"))?;
    Ok(())
}

/// That the store's prefill, with `done` archives in, is still this one: the
/// same genesis file, the same number of archives, and the last committed
/// archive still the one in its place.
fn check(store: &HostStore, prefill: &Prefill, done: u64) -> Result<()> {
    let head = store.head();
    let counters = prefill::read(&**store, ReadTarget::Commit(head))
        .map_err(|e| eyre!("read the prefill record: {e:?}"))?
        .ok_or_else(|| eyre!("the store has no prefill record"))?;
    ensure!(
        counters.genesis_hash == prefill.genesis_hash,
        "the genesis file changed since this prefill started: it hashes to {}, but the prefill \
         started with one hashing to {}. Use the original file, or start again in an empty \
         directory",
        B256::from(prefill.genesis_hash),
        B256::from(counters.genesis_hash)
    );
    let given = prefill.archives.len() as u64;
    ensure!(
        counters.total == given,
        "this store was started with {} archives; {given} were given",
        counters.total
    );
    let Some(last) = done.checked_sub(1) else {
        return Ok(());
    };
    let archive = &prefill.archives[last as usize];
    let expected = archive.hash()?;
    let tag = store
        .tag_of(head)
        .map_err(|e| eyre!("read commit {}'s tag: {e:?}", head.0))?;
    ensure!(
        tag == Some(expected.0),
        "commit {} holds an archive hashing to {}, but archive {} of the list, {}, hashes to \
         {expected}: the list changed since this prefill started",
        head.0,
        tag.map_or_else(|| "nothing".to_owned(), |tag| B256::from(tag).to_string()),
        done,
        archive.index().display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{ALICE, CHAIN, line, store};
    use alloy_primitives::U256;
    use std::path::{Path, PathBuf};

    const GENESIS: [u8; 32] = [0x4f; 32];

    fn alice() -> Vec<GenesisAccount> {
        vec![GenesisAccount {
            address: ALICE.parse().unwrap(),
            balance: U256::from(100),
            nonce: 0,
        }]
    }

    /// Three archives in `dir`, `A0`…`A2`, two entities each, every key
    /// distinct.
    fn archives(dir: &Path) -> Vec<Archive> {
        (0..3u8)
            .map(|n| {
                let at = dir.join(format!("A{n}"));
                std::fs::create_dir_all(&at).unwrap();
                let lines = [
                    line(2 * n + 1, ALICE, ALICE, 1_000, ""),
                    line(2 * n + 2, ALICE, ALICE, 2_000, ""),
                ];
                std::fs::write(at.join("p.jsonl"), lines.join("\n") + "\n").unwrap();
                std::fs::write(
                    at.join("index.json"),
                    r#"{"version":1,"parts":["p.jsonl"]}"#,
                )
                .unwrap();
                Archive::open(at.join("index.json")).unwrap()
            })
            .collect()
    }

    fn plan<'a>(alloc: &'a [GenesisAccount], archives: &'a [Archive]) -> Prefill<'a> {
        Prefill {
            chain_id: CHAIN,
            genesis_hash: GENESIS,
            alloc,
            archives,
        }
    }

    fn root(store: &HostStore) -> [u8; 32] {
        store.commit_hash(store.head()).unwrap()
    }

    /// A clean run on a fresh store: every archive in, ready to finalize.
    fn clean_root() -> [u8; 32] {
        let dir = tempfile::tempdir().unwrap();
        let archives = archives(dir.path());
        let store = store();
        let ran = run(&store, &plan(&alice(), &archives), |_, _| {}).unwrap();
        assert_eq!(ran.resumed_from, 0);
        assert_eq!(ran.written.len(), 3);
        assert_eq!(
            classify(&store, NO_BLOCK_ZERO).unwrap(),
            StoreState::ArkivPrefillDone
        );
        root(&store)
    }

    /// `A{n}`'s part, moved aside so that reading it fails, and the path to
    /// put it back from.
    fn break_archive(dir: &Path, n: usize) -> (PathBuf, PathBuf) {
        let part = dir.join(format!("A{n}/p.jsonl"));
        let aside = dir.join(format!("A{n}/p.jsonl.aside"));
        std::fs::rename(&part, &aside).unwrap();
        (aside, part)
    }

    /// The same archives in two stores give the same root.
    #[test]
    fn the_same_archives_give_the_same_root() {
        assert_eq!(clean_root(), clean_root());
    }

    /// A run that stops after any number of archives, then resumes, ends at
    /// the same root as a run that never stopped, and writes each archive
    /// once.
    #[test]
    fn a_resumed_run_ends_where_a_clean_one_does() {
        let clean = clean_root();
        for stop in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let archives = archives(dir.path());
            let alloc = alice();
            let store = store();
            let (aside, part) = break_archive(dir.path(), stop);
            assert!(run(&store, &plan(&alloc, &archives), |_, _| {}).is_err());
            assert_eq!(
                classify(&store, NO_BLOCK_ZERO).unwrap(),
                StoreState::ArkivPrefilling {
                    done: stop as u64,
                    total: 3
                },
                "stopped before A{stop}"
            );
            std::fs::rename(aside, part).unwrap();
            let mut places = Vec::new();
            let ran = run(&store, &plan(&alloc, &archives), |place, _| {
                places.push(place)
            })
            .unwrap();
            assert_eq!(ran.resumed_from, stop as u64);
            assert_eq!(places, (stop..3).collect::<Vec<_>>(), "each archive once");
            assert_eq!(root(&store), clean, "stopped before A{stop}");
        }
    }

    /// A prefill stopped after two archives, ready to resume.
    fn stopped_after_two() -> (tempfile::TempDir, Vec<Archive>, HostStore) {
        let dir = tempfile::tempdir().unwrap();
        let archives = archives(dir.path());
        let store = store();
        let (aside, part) = break_archive(dir.path(), 2);
        assert!(run(&store, &plan(&alice(), &archives), |_, _| {}).is_err());
        std::fs::rename(aside, part).unwrap();
        (dir, archives, store)
    }

    fn refused(store: &HostStore, prefill: &Prefill) -> String {
        let head = store.head();
        let error = run(store, prefill, |_, _| {}).unwrap_err().to_string();
        assert_eq!(store.head(), head, "a refusal writes nothing");
        error
    }

    #[test]
    fn a_reordered_list_is_refused() {
        let (_dir, mut archives, store) = stopped_after_two();
        archives.swap(1, 2);
        let error = refused(&store, &plan(&alice(), &archives));
        assert!(
            error.starts_with("commit 3 holds an archive hashing to 0x"),
            "{error}"
        );
        assert!(error.contains("but archive 2 of the list"), "{error}");
    }

    #[test]
    fn another_number_of_archives_is_refused() {
        let (_dir, archives, store) = stopped_after_two();
        let error = refused(&store, &plan(&alice(), &archives[..2]));
        assert_eq!(
            error,
            "this store was started with 3 archives; 2 were given"
        );
    }

    #[test]
    fn a_changed_genesis_file_is_refused() {
        let (_dir, archives, store) = stopped_after_two();
        let alloc = alice();
        let mut edited = plan(&alloc, &archives);
        edited.genesis_hash = [0x99; 32];
        let error = refused(&store, &edited);
        assert!(
            error.starts_with("the genesis file changed since this prefill started"),
            "{error}"
        );
    }

    /// A finished prefill has nothing left to write, but the checks still
    /// run: a changed list is refused rather than reported as done.
    #[test]
    fn a_finished_prefill_writes_nothing_more() {
        let dir = tempfile::tempdir().unwrap();
        let mut archives = archives(dir.path());
        let alloc = alice();
        let store = store();
        run(&store, &plan(&alloc, &archives), |_, _| {}).unwrap();
        let head = store.head();
        let again = run(&store, &plan(&alloc, &archives), |_, _| {}).unwrap();
        assert_eq!((again.resumed_from, again.written.len()), (3, 0));
        assert_eq!(store.head(), head);
        archives.swap(0, 2);
        assert!(refused(&store, &plan(&alloc, &archives)).contains("the list changed"));
    }

    /// A store that runs a chain, not a prefill, is refused.
    #[test]
    fn another_chains_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archives = archives(dir.path());
        let alloc = alice();
        let store = store();
        arkiv_reth_statemanager::seed_genesis(&store, [0xb0; 32], alloc.iter().copied()).unwrap();
        let error = refused(&store, &plan(&alloc, &archives));
        assert!(
            error.starts_with("the store holds another chain"),
            "{error}"
        );
    }
}
