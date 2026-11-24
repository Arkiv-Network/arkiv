//! Finalizing a prefill: the commit that becomes block 0.
//!
//! Once every archive is in, block 0 is one more commit, with nothing in it of
//! its own. The order works out although that commit is tagged with a hash
//! that depends on its own root, because tags sit outside the root:
//!
//! ```text
//! seal the empty commit → GolemDB's root, 0x5e9c…
//!   → genesis file gets "prefilledBlockZeroStateRoot": "0x5e9c…"
//!   → block 0's header, and its hash 0x1d44…
//!   → commit, tagged 0x1d44…          (the root does not change)
//! ```
//!
//! [`finalize`] does the store's part and asks its caller for block 0's hash,
//! so the hash is computed exactly as the node computes it from the genesis
//! file. [`set_prefilled_root`] writes the root into the genesis file.

use alloy_primitives::B256;
use arkiv_golemdb_state::prefill;
use arkiv_interfaces::store::{CommitId, ReadTarget, Store, StoreExt};
use arkiv_reth_statemanager::{HostStore, StoreState, classify};
use eyre::{Result, bail, ensure, eyre};
use serde_json::Value;

use crate::run::{NO_BLOCK_ZERO, check_genesis};

/// What finalizing produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finalized {
    /// Block 0's state root: what the genesis file's
    /// `prefilledBlockZeroStateRoot` says.
    pub root: B256,
    /// Block 0's hash, which the commit is tagged with.
    pub block_zero: B256,
    /// The commit that is block 0.
    pub commit: CommitId,
}

/// Finalize the prefill in `store`: commit block 0 on top of the last archive.
///
/// - `genesis_hash` is the genesis file's prefill hash
///   (`ArkivChainSpec::prefill_genesis_hash`), which must be the one the
///   prefill started with.
/// - `expected_root` is the file's `prefilledBlockZeroStateRoot`, if it
///   already has one. That is an audit run: someone checking the published
///   root from the published archives, who is refused if the root differs.
/// - `block_zero_of` gives block 0's hash for a root: the genesis file's header
///   with that root, as the node builds it.
///
/// Only a store with every archive in is finalized. On any error nothing is
/// committed, and the store stays ready to finalize.
pub fn finalize(
    store: &HostStore,
    genesis_hash: [u8; 32],
    expected_root: Option<B256>,
    block_zero_of: impl FnOnce(B256) -> Result<B256>,
) -> Result<Finalized> {
    match classify(store, NO_BLOCK_ZERO)? {
        StoreState::ArkivPrefillDone => {}
        StoreState::ArkivPrefilling { done, total } => bail!(
            "{done} of {total} archives are in: run the prefill to finish it before finalizing"
        ),
        StoreState::GolemDbInitialized => {
            bail!("the store is empty: run the prefill before finalizing")
        }
        StoreState::ArkivInitialized => bail!("the store's prefill is already finalized"),
        StoreState::ArkivPrefilledOtherBlockZero => {
            // Name the root, so a run that died before writing it into the
            // genesis file can still be finished without prefilling again.
            let (root, block_zero) = finalized_block_zero(store)?;
            bail!(
                "the store's prefill is already finalized: block 0's root is {root}, its hash \
                 {block_zero}"
            )
        }
        StoreState::ArkivPrefillInconsistent { done, total, head } => bail!(
            "the store's prefill record does not fit its commits: it counts {done} of {total} \
             archives, but the head is commit {head}. The store is damaged"
        ),
        StoreState::ForeignChain => bail!("the store holds another chain, not a prefill"),
    }
    let counters = prefill::read(&**store, ReadTarget::Commit(store.head()))
        .map_err(|e| eyre!("read the prefill record: {e:?}"))?
        .ok_or_else(|| eyre!("the store has no prefill record"))?;
    check_genesis(counters.genesis_hash, genesis_hash)?;

    let branch = store
        .begin(None)
        .map_err(|e| eyre!("open block 0's commit: {e:?}"))?;
    match seal_and_commit(store, branch, expected_root, block_zero_of) {
        Ok(finalized) => Ok(finalized),
        Err(error) => {
            let _ = store.discard(branch);
            Err(error)
        }
    }
}

/// A finalized store's block 0: its root and the hash it is tagged with. It is
/// the commit after the last archive, `total + 2`.
fn finalized_block_zero(store: &HostStore) -> Result<(B256, B256)> {
    let counters = prefill::read(&**store, ReadTarget::Commit(store.head()))
        .map_err(|e| eyre!("read the prefill record: {e:?}"))?
        .ok_or_else(|| eyre!("the store has no prefill record"))?;
    let commit = CommitId(counters.total + 2);
    let root = store
        .commit_hash(commit)
        .map_err(|e| eyre!("read block 0's root: {e:?}"))?;
    let tag = store
        .tag_of(commit)
        .map_err(|e| eyre!("read block 0's tag: {e:?}"))?
        .ok_or_else(|| eyre!("commit {} has no block 0 tag", commit.0))?;
    Ok((B256::from(root), B256::from(tag)))
}

fn seal_and_commit(
    store: &HostStore,
    branch: arkiv_interfaces::store::BranchId,
    expected_root: Option<B256>,
    block_zero_of: impl FnOnce(B256) -> Result<B256>,
) -> Result<Finalized> {
    let sealed = store
        .seal(branch)
        .map_err(|e| eyre!("seal block 0's commit: {e:?}"))?;
    let root = B256::from(sealed.state_root);
    if let Some(expected) = expected_root {
        ensure!(
            expected == root,
            "these archives don't produce this chain's block 0: their root is {root}, but the \
             genesis file's prefilledBlockZeroStateRoot is {expected}"
        );
    }
    let block_zero = block_zero_of(root)?;
    let commit = store
        .commit_tagged(branch, block_zero.0)
        .map_err(|e| eyre!("commit block 0: {e:?}"))?;
    // The tag sits outside the root, so committing it cannot have moved it.
    let committed = store
        .commit_hash(commit)
        .map_err(|e| eyre!("read block 0's root: {e:?}"))?;
    ensure!(
        committed == root.0,
        "block 0 committed with root {}, not the sealed {root}",
        B256::from(committed)
    );
    Ok(Finalized {
        root,
        block_zero,
        commit,
    })
}

/// The genesis file `genesis` with `config.arkiv.prefilledBlockZeroStateRoot`
/// set to `root`, written out again.
///
/// Nothing else changes in what the file says, so its prefill hash stays the
/// same: that hash leaves the field out and does not depend on layout.
/// Refused for a file with no `config.arkiv` section, which is no GolemDB chain,
/// for one whose field already holds another root, and for one with a number
/// that does not fit 64 bits, which JSON parsing rounds and so could not be
/// written back as it was.
pub fn set_prefilled_root(genesis: &str, root: B256) -> Result<String> {
    let mut file: Value =
        serde_json::from_str(genesis).map_err(|e| eyre!("parse the genesis file: {e}"))?;
    if let Some(at) = inexact_number(&file, String::new()) {
        bail!(
            "the genesis file's {at} is not a whole number that fits 64 bits, so it cannot be \
             written back exactly: write it as a hex string"
        );
    }
    let arkiv = file
        .pointer_mut("/config/arkiv")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            eyre!("the genesis file has no config.arkiv section: it is not a chain on GolemDB")
        })?;
    let field = "prefilledBlockZeroStateRoot";
    if let Some(existing) = arkiv.get(field) {
        ensure!(
            existing.as_str().and_then(|s| s.parse::<B256>().ok()) == Some(root),
            "the genesis file's {field} is already {existing}, not {root}"
        );
    }
    arkiv.insert(field.to_owned(), Value::String(root.to_string()));
    let mut text = serde_json::to_string_pretty(&file).expect("JSON serializes");
    text.push('\n');
    Ok(text)
}

/// The JSON pointer of the first number in `value` that parsed as a float: a
/// fraction, or an integer too large for `u64`/`i64` and already rounded.
fn inexact_number(value: &Value, at: String) -> Option<String> {
    match value {
        Value::Number(n) if n.is_f64() => Some(at),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| inexact_number(v, format!("{at}/{i}"))),
        Value::Object(fields) => fields
            .iter()
            .find_map(|(k, v)| inexact_number(v, format!("{at}/{k}"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{ALICE, CHAIN, line, store};
    use crate::{Archive, Prefill, run};
    use alloy_primitives::{U256, keccak256};
    use arkiv_reth_chainspec::ArkivChainSpec;
    use arkiv_reth_statemanager::{GenesisAccount, check_genesis_root, prepare_genesis};

    const GENESIS: [u8; 32] = [0x4f; 32];

    fn alice() -> Vec<GenesisAccount> {
        vec![GenesisAccount {
            address: ALICE.parse().unwrap(),
            balance: U256::from(100),
            nonce: 0,
        }]
    }

    /// A store with `done` of `total` archives (one entity each) prefilled,
    /// not finalized.
    fn prefilled(done: usize, total: usize) -> (tempfile::TempDir, HostStore) {
        let dir = tempfile::tempdir().unwrap();
        let archives: Vec<Archive> = (0..total)
            .map(|n| {
                let at = dir.path().join(format!("A{n}"));
                std::fs::create_dir_all(&at).unwrap();
                std::fs::write(
                    at.join("p.jsonl"),
                    line(n as u8 + 1, ALICE, ALICE, 1_000, "") + "\n",
                )
                .unwrap();
                std::fs::write(
                    at.join("index.json"),
                    r#"{"version":1,"parts":["p.jsonl"]}"#,
                )
                .unwrap();
                Archive::open(at.join("index.json")).unwrap()
            })
            .collect();
        let store = store();
        if done < total {
            std::fs::remove_file(dir.path().join(format!("A{done}/p.jsonl"))).unwrap();
        }
        let alloc = alice();
        let plan = Prefill {
            chain_id: CHAIN,
            genesis_hash: GENESIS,
            alloc: &alloc,
            archives: &archives,
        };
        let ran = run(&store, &plan, |_, _| {});
        assert_eq!(ran.is_ok(), done == total);
        (dir, store)
    }

    /// A stand-in for the genesis file's header: any hash that depends on the
    /// root.
    fn block_zero(root: B256) -> Result<B256> {
        Ok(keccak256(root))
    }

    /// After finalizing, the node starts on the store: block 0 is the tagged
    /// commit, its root is the sealed one, and the prefill record still
    /// matches the genesis file.
    #[test]
    fn a_finalized_store_is_one_the_node_starts_on() {
        let (_dir, store) = prefilled(2, 2);
        let head = store.head();
        let finalized = finalize(&store, GENESIS, None, block_zero).unwrap();
        assert_eq!(finalized.commit.0, head.0 + 1, "one empty commit on top");
        assert_eq!(finalized.block_zero, keccak256(finalized.root));
        assert_eq!(
            store.commit_by_tag(finalized.block_zero.0).unwrap(),
            Some(finalized.commit)
        );
        assert_eq!(
            classify(&store, finalized.block_zero.0).unwrap(),
            StoreState::ArkivInitialized
        );
        assert_eq!(
            prepare_genesis(&store, finalized.block_zero.0, Some(GENESIS), alice()).unwrap(),
            None
        );
        check_genesis_root(&store, finalized.block_zero.0, finalized.root.0).unwrap();
    }

    /// An audit run with the published root gets the same block 0; one whose
    /// archives give another root is refused and commits nothing.
    #[test]
    fn an_audit_run_checks_the_published_root() {
        let (_dir, published) = prefilled(2, 2);
        let root = finalize(&published, GENESIS, None, block_zero)
            .unwrap()
            .root;

        let (_dir, audit) = prefilled(2, 2);
        let again = finalize(&audit, GENESIS, Some(root), block_zero).unwrap();
        assert_eq!(again.root, root);

        let (_dir, other) = prefilled(2, 2);
        let head = other.head();
        let error = finalize(&other, GENESIS, Some(B256::repeat_byte(9)), block_zero)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("these archives don't produce this chain's block 0"),
            "{error}"
        );
        assert_eq!(other.head(), head, "nothing committed");
    }

    #[test]
    fn only_a_finished_prefill_is_finalized() {
        let (_dir, partway) = prefilled(1, 2);
        let error = finalize(&partway, GENESIS, None, block_zero)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "1 of 2 archives are in: run the prefill to finish it before finalizing"
        );

        let (_dir, done) = prefilled(2, 2);
        let finalized = finalize(&done, GENESIS, None, block_zero).unwrap();
        let error = finalize(&done, GENESIS, None, block_zero)
            .unwrap_err()
            .to_string();
        // The root is named, for a run that died before writing it out.
        assert_eq!(
            error,
            format!(
                "the store's prefill is already finalized: block 0's root is {}, its hash {}",
                finalized.root, finalized.block_zero
            )
        );

        let error = finalize(&store(), GENESIS, None, block_zero)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "the store is empty: run the prefill before finalizing"
        );
    }

    #[test]
    fn a_changed_genesis_file_is_refused() {
        let (_dir, store) = prefilled(2, 2);
        let head = store.head();
        let error = finalize(&store, [0x99; 32], None, block_zero)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("the genesis file changed since this prefill started"),
            "{error}"
        );
        assert_eq!(store.head(), head);
    }

    /// A failure computing block 0's hash commits nothing.
    #[test]
    fn a_failed_block_zero_commits_nothing() {
        let (_dir, store) = prefilled(2, 2);
        let head = store.head();
        let error = finalize(&store, GENESIS, None, |_| Err(eyre!("no header")))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "no header");
        assert_eq!(store.head(), head);
        // And the store can still be finalized.
        finalize(&store, GENESIS, None, block_zero).unwrap();
    }

    /// Writing the root into the genesis file adds the field and changes
    /// nothing else it says: the prefill hash stays, and the chain spec reads
    /// the root back.
    #[test]
    fn the_root_goes_into_the_genesis_file() {
        let original = include_str!("../../../genesis/example.json");
        let root = B256::repeat_byte(0x5e);
        let written = set_prefilled_root(original, root).unwrap();
        let spec =
            |text: &str| ArkivChainSpec::from_genesis(serde_json::from_str(text).unwrap()).unwrap();
        let (before, after) = (spec(original), spec(&written));
        assert_eq!(after.prefilled_block_zero_state_root(), Some(root));
        assert_eq!(before.prefill_genesis_hash(), after.prefill_genesis_hash());

        // Again with the same root is fine; another root is refused.
        assert_eq!(set_prefilled_root(&written, root).unwrap(), written);
        // The same root in capitals is the same root.
        let upper = written.replace(
            &root.to_string()[2..],
            &root.to_string()[2..].to_uppercase(),
        );
        assert_ne!(upper, written);
        assert_eq!(set_prefilled_root(&upper, root).unwrap(), written);
        let error = set_prefilled_root(&written, B256::repeat_byte(1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("is already"), "{error}");
        // A file for no GolemDB chain has nowhere to put it.
        let error = set_prefilled_root(r#"{"config":{"chainId":1}}"#, root)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no config.arkiv section"), "{error}");
        // A number past u64 would come back rounded, so it is refused.
        let big = r#"{"config":{"arkiv":{},"terminalTotalDifficulty":58750000000000000000000}}"#;
        let error = set_prefilled_root(big, root).unwrap_err().to_string();
        assert!(
            error.contains("/config/terminalTotalDifficulty is not a whole number"),
            "{error}"
        );
    }
}
