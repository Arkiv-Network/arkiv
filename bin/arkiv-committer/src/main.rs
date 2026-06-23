//! Thin entrypoint for the committer — parse config, hand off to the library.

use arkiv_committer::CommitterConfig;
use clap::Parser;

fn main() -> eyre::Result<()> {
    arkiv_committer::run(CommitterConfig::parse())
}
