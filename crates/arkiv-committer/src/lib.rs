//! arkiv-committer — posts the sequencer's canonical blocks to the base-chain
//! inbox as DA (the Committer in `docs/september-action-plan.md` §6, owner Piotr).
//!
//! This is **leg-work, not the committer**: config + the DA seam are wired so the
//! real poll/encode/post loop drops straight in. `run` validates the DA round-trip
//! and then heartbeats so the service slots into the kurtosis network alongside
//! the EL-CL + base-chain pair.

use clap::Parser;
use std::time::Duration;

/// Where the committer reads blocks from and posts DA to.
///
/// Defaults target the in-enclave kurtosis service DNS, so the image runs
/// unmodified inside the network; override via flags/env to run on the host.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "arkiv-committer",
    about = "Post arkiv-node blocks to the base-chain inbox as DA"
)]
pub struct CommitterConfig {
    /// Sequencer EL JSON-RPC (`eth_*`) — the canonical block source.
    #[arg(
        long,
        env = "ARKIV_SEQUENCER_RPC",
        default_value = "http://el-1-reth-lighthouse:8545"
    )]
    pub sequencer_rpc: String,

    /// Base-chain JSON-RPC — where the inbox contract lives.
    #[arg(
        long,
        env = "ARKIV_BASE_CHAIN_RPC",
        default_value = "http://base-chain:8545"
    )]
    pub base_chain_rpc: String,

    /// Inbox contract address — target of `postBlock(bytes)`.
    #[arg(
        long,
        env = "ARKIV_INBOX_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub inbox_address: String,

    /// Seconds between sequencer polls.
    #[arg(long, env = "ARKIV_POLL_INTERVAL", default_value_t = 2)]
    pub poll_interval_secs: u64,

    /// First sequencer block to commit.
    #[arg(long, env = "ARKIV_START_BLOCK", default_value_t = 0)]
    pub start_block: u64,
}

/// Log the resolved topology, self-check the DA seam, then heartbeat.
pub fn run(cfg: CommitterConfig) -> eyre::Result<()> {
    println!("arkiv-committer v{}", env!("CARGO_PKG_VERSION"));
    println!("  sequencer RPC : {}", cfg.sequencer_rpc);
    println!("  base chain RPC: {}", cfg.base_chain_rpc);
    println!("  inbox address : {}", cfg.inbox_address);
    println!(
        "  DA format     : v{} (zstd level {})",
        arkiv_da::DA_VERSION,
        arkiv_da::ZSTD_LEVEL
    );
    println!(
        "  poll interval : {}s, start block {}",
        cfg.poll_interval_secs, cfg.start_block
    );

    self_check()?;
    println!("DA self-check OK — committer is a STUB; block posting not yet implemented.");

    // TODO(committer, Piotr): replace this heartbeat with the real loop —
    //   poll eth_getBlockByNumber(start_block..) on sequencer_rpc
    //     -> arkiv_da::encode_block(&block)
    //     -> send postBlock(payload) tx to inbox_address on base_chain_rpc.
    let interval = Duration::from_secs(cfg.poll_interval_secs.max(1));
    loop {
        std::thread::sleep(interval);
    }
}

/// Prove the DA seam links and round-trips before the service reports healthy.
fn self_check() -> eyre::Result<()> {
    const SAMPLE: &[u8] = b"arkiv-da self-check";
    let payload = arkiv_da::encode_bytes(SAMPLE);
    let back = arkiv_da::decode_bytes(&payload).map_err(|e| eyre::eyre!("DA self-check: {e}"))?;
    eyre::ensure!(back == SAMPLE, "DA self-check round-trip mismatch");
    Ok(())
}
