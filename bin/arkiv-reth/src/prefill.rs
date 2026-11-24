//! `arkiv-reth prefill`: write a chain's prefill into GolemDB before the
//! chain starts, then commit block 0 on top of it.
//!
//! ```text
//! arkiv-reth prefill --chain genesis.json --archives set.json              # the archives; resumable
//! arkiv-reth prefill --chain genesis.json --archives set.json --finalize   # block 0, and its root into genesis.json
//! arkiv-reth node    --chain genesis.json                                  # block 1 onwards
//! ```
//!
//! The store is GolemDB's directory, `--store` or `ARKIV_STORE_PATH` as for the
//! node, opened with the same settings from the genesis file, so the roots the
//! node later checks are the roots written here.
//!
//! reth's command line has a fixed set of subcommands, so this one is picked
//! out before reth parses the rest: `arkiv-reth prefill …` never reaches it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use arkiv_prefill::{Prefill, finalize, open_set, run, set_prefilled_root};
use arkiv_reth_chainspec::{ArkivChainSpec, ArkivChainSpecParser};
use arkiv_reth_statemanager::{GenesisAccount, HostStore, StoreState, classify};
use clap::Parser;
use eyre::{Context, bail, ensure};
use reth_cli::chainspec::ChainSpecParser;
use reth_ethereum::chainspec::EthChainSpec;

use crate::genesis::NodeChainSpecParser;

/// Write a chain's prefill into GolemDB, then finalize block 0.
#[derive(Debug, Parser)]
#[command(name = "arkiv-reth prefill")]
pub(crate) struct PrefillCommand {
    /// The chain's genesis file. `--finalize` writes block 0's state root into
    /// it, as `config.arkiv.prefilledBlockZeroStateRoot`.
    #[arg(long, value_name = "FILE")]
    chain: PathBuf,
    /// The archive set file: the archives' index files, in commit order.
    #[arg(long, value_name = "FILE")]
    archives: PathBuf,
    /// Commit block 0 instead of archives. Only once every archive is in.
    #[arg(long)]
    finalize: bool,
    /// GolemDB's directory, as the node takes it.
    #[arg(long, env = "ARKIV_STORE_PATH", value_name = "DIR")]
    store: OsString,
}

/// Run `arkiv-reth prefill` with `args`, the words after `prefill`.
pub(crate) fn main(args: impl IntoIterator<Item = OsString>) -> eyre::Result<()> {
    let command = PrefillCommand::parse_from(
        std::iter::once(OsString::from("arkiv-reth prefill")).chain(args),
    );
    command.execute(&mut std::io::stdout())
}

impl PrefillCommand {
    /// Run, reporting progress to `out`.
    fn execute(&self, out: &mut impl std::io::Write) -> eyre::Result<()> {
        let chain = self.chain.display();
        let text = std::fs::read_to_string(&self.chain)
            .wrap_err_with(|| format!("read the genesis file {chain}"))?;
        let spec = ArkivChainSpecParser::parse(&text)
            .wrap_err_with(|| format!("parse the genesis file {chain}"))?;
        let archives = open_set(&self.archives)?;
        let store = crate::open_store(&spec, Some(self.store.clone()), None)?;
        let count = match archives.len() {
            1 => "1 archive".to_owned(),
            n => format!("{n} archives"),
        };
        writeln!(
            out,
            "chain {}: {count} in {}",
            spec.chain().id(),
            self.archives.display()
        )?;
        if self.finalize {
            self.finalize(out, &text, &spec, &store)
        } else {
            self.run(out, &spec, &store, &archives)
        }
    }

    fn run(
        &self,
        out: &mut impl std::io::Write,
        spec: &ArkivChainSpec,
        store: &HostStore,
        archives: &[arkiv_prefill::Archive],
    ) -> eyre::Result<()> {
        let total = archives.len();
        match classify(store, [0; 32])? {
            StoreState::GolemDbInitialized => {
                writeln!(
                    out,
                    "store: empty; writing the genesis accounts, then the archives"
                )?;
            }
            StoreState::ArkivPrefilling { done, total } => writeln!(
                out,
                "store: {done} of {total} archives in; checking nothing changed, then resuming"
            )?,
            StoreState::ArkivPrefillDone => {
                writeln!(out, "store: every archive is in; checking nothing changed")?;
            }
            // `run` refuses these, saying why.
            _ => {}
        }
        let alloc: Vec<GenesisAccount> =
            GenesisAccount::from_alloc(&spec.genesis().alloc).collect();
        let prefill = Prefill {
            chain_id: spec.chain().id(),
            genesis_hash: spec.prefill_genesis_hash().0,
            alloc: &alloc,
            archives,
        };
        let mut since = Instant::now();
        let mut lines = Vec::new();
        let ran = run(store, &prefill, |place, written| {
            let entities = match written.entities {
                1 => "1 entity".to_owned(),
                n => format!("{n} entities"),
            };
            lines.push(format!(
                "{}  committed as commit {} · {entities} · {:.1} s · {}",
                archives[place].index().display(),
                written.commit.0,
                since.elapsed().as_secs_f64(),
                written.hash
            ));
            since = Instant::now();
        });
        for line in &lines {
            writeln!(out, "{line}")?;
        }
        ran?;
        writeln!(
            out,
            "{total} of {total} archives in. Run again with --finalize to commit block 0."
        )?;
        Ok(())
    }

    fn finalize(
        &self,
        out: &mut impl std::io::Write,
        text: &str,
        spec: &ArkivChainSpec,
        store: &HostStore,
    ) -> eyre::Result<()> {
        let published = spec.prefilled_block_zero_state_root();
        let mut with_root = None;
        let finalized = finalize(store, spec.prefill_genesis_hash().0, published, |root| {
            let text = set_prefilled_root(text, root)?;
            // Block 0's hash exactly as the node computes it from the file.
            let hash = NodeChainSpecParser::parse(&text)?.genesis_hash();
            with_root = Some(text);
            Ok(hash)
        })?;
        writeln!(
            out,
            "block 0 committed as commit {}: state root {}, hash {}",
            finalized.commit.0, finalized.root, finalized.block_zero
        )?;
        let chain = self.chain.display();
        if published.is_some() {
            writeln!(out, "it matches {chain}'s prefilledBlockZeroStateRoot")?;
        } else {
            let text = with_root.expect("finalize asks for block 0's hash before committing");
            if let Err(error) = replace(&self.chain, &text) {
                bail!(
                    "block 0 is committed, but writing {chain} failed: {error:#}. Add \
                     \"prefilledBlockZeroStateRoot\": \"{}\" under config.arkiv by hand",
                    finalized.root
                );
            }
            writeln!(
                out,
                "wrote config.arkiv.prefilledBlockZeroStateRoot = {} into {chain}",
                finalized.root
            )?;
        }
        writeln!(out, "Start the chain with: arkiv-reth node --chain {chain}")?;
        Ok(())
    }
}

/// Replace `path`'s contents with `text` in one step: written beside it, then
/// renamed over it, so a crash leaves the old file or the new one, never half
/// of one.
fn replace(path: &Path, text: &str) -> eyre::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".prefill-tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, text).wrap_err_with(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .wrap_err_with(|| format!("rename onto {}", path.display()))?;
    ensure!(
        std::fs::read_to_string(path)? == text,
        "{} did not take",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_reth_statemanager::{check_genesis_root, prepare_genesis};

    const ALICE: &str = "0x1111111111111111111111111111111111111111";

    /// A prefill directory: the example genesis file, two archives of two
    /// entities each and their set file, and a store directory.
    struct Setup {
        dir: tempfile::TempDir,
    }

    impl Setup {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("genesis.json"),
                include_str!("../../../genesis/example.json"),
            )
            .unwrap();
            for n in 0..2u8 {
                let at = dir.path().join(format!("A{n}"));
                std::fs::create_dir(&at).unwrap();
                let lines: Vec<String> = (1..=2u8)
                    .map(|k| {
                        format!(
                            r#"{{"$key":"0x{}","$creator":"{ALICE}","$owner":"{ALICE}","$expiresAt":5000,"kind":"str:k{k}"}}"#,
                            format!("{:02x}", 10 * n + k).repeat(32)
                        )
                    })
                    .collect();
                std::fs::write(at.join("p.jsonl"), lines.join("\n") + "\n").unwrap();
                std::fs::write(
                    at.join("index.json"),
                    r#"{"version":1,"parts":["p.jsonl"]}"#,
                )
                .unwrap();
            }
            std::fs::write(
                dir.path().join("set.json"),
                r#"{"version":1,"archives":["A0/index.json","A1/index.json"]}"#,
            )
            .unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn command(&self, store: &str, finalize: bool) -> PrefillCommand {
            PrefillCommand {
                chain: self.path("genesis.json"),
                archives: self.path("set.json"),
                finalize,
                store: self.path(store).into_os_string(),
            }
        }

        /// Run the command; what it printed, or its error.
        fn run(&self, store: &str, finalize: bool) -> Result<String, String> {
            let mut out = Vec::new();
            let result = self.command(store, finalize).execute(&mut out);
            let printed = String::from_utf8(out).unwrap();
            result.map(|()| printed).map_err(|e| format!("{e:#}"))
        }

        fn genesis(&self) -> String {
            std::fs::read_to_string(self.path("genesis.json")).unwrap()
        }
    }

    /// Prefill, finalize, and the node accepts the store under the genesis
    /// file the finalize wrote: block 0's hash, as the node computes it from
    /// that file, tags the store's block 0, whose root the file carries.
    #[test]
    fn prefill_then_finalize_gives_a_store_the_node_starts_on() {
        let setup = Setup::new();
        let printed = setup.run("golemdb", false).unwrap();
        assert!(printed.contains("store: empty"), "{printed}");
        assert!(
            printed.contains("A1/index.json  committed as commit 3 · 2 entities"),
            "{printed}"
        );
        assert!(
            printed.ends_with("2 of 2 archives in. Run again with --finalize to commit block 0.\n")
        );

        let printed = setup.run("golemdb", true).unwrap();
        assert!(
            printed.contains("block 0 committed as commit 4"),
            "{printed}"
        );
        assert!(
            printed.contains("wrote config.arkiv.prefilledBlockZeroStateRoot"),
            "{printed}"
        );

        let genesis = setup.genesis();
        let node = NodeChainSpecParser::parse(&genesis).unwrap();
        let root = node
            .prefilled_block_zero_state_root()
            .expect("the field was written");
        assert_eq!(node.genesis_header().state_root, root);
        let store =
            crate::open_store(&node, Some(setup.path("golemdb").into_os_string()), None).unwrap();
        let block_zero = node.genesis_hash().0;
        assert_eq!(
            prepare_genesis(&store, block_zero, Some(node.prefill_genesis_hash().0), []).unwrap(),
            None
        );
        check_genesis_root(&store, block_zero, root.0).unwrap();
    }

    /// Run again after it finished, the prefill writes nothing more; finalized,
    /// it is refused.
    #[test]
    fn a_second_run_writes_nothing_and_a_second_finalize_is_refused() {
        let setup = Setup::new();
        setup.run("golemdb", false).unwrap();
        let printed = setup.run("golemdb", false).unwrap();
        assert!(printed.contains("store: every archive is in"), "{printed}");
        assert!(!printed.contains("committed as commit"), "{printed}");
        setup.run("golemdb", true).unwrap();
        let error = setup.run("golemdb", true).unwrap_err();
        assert!(error.contains("already finalized"), "{error}");
    }

    #[test]
    fn finalize_before_the_archives_is_refused() {
        let setup = Setup::new();
        let error = setup.run("golemdb", true).unwrap_err();
        assert_eq!(
            error,
            "the store is empty: run the prefill before finalizing"
        );
        assert!(!setup.genesis().contains("prefilledBlockZeroStateRoot"));
    }

    /// Anyone can re-run the published archives against the published genesis
    /// file: same root, and the file is left as it is.
    #[test]
    fn an_audit_run_matches_the_published_root() {
        let setup = Setup::new();
        setup.run("golemdb", false).unwrap();
        setup.run("golemdb", true).unwrap();
        let published = setup.genesis();

        setup.run("audit", false).unwrap();
        let printed = setup.run("audit", true).unwrap();
        assert!(printed.contains("it matches"), "{printed}");
        assert_eq!(setup.genesis(), published, "the file is not rewritten");
    }

    /// The genesis file is replaced whole: nothing is left beside it.
    #[test]
    fn the_genesis_file_is_replaced_in_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("genesis.json");
        std::fs::write(&file, "old").unwrap();
        replace(&file, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
