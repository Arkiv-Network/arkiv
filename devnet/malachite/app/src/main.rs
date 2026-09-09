//! Local Arkiv consensus application using released Malachite crates.
use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, ensure};
use malachitebft_app::node::Node;
use malachitebft_config::{
    ConsensusConfig, MetricsConfig, P2pConfig, ValuePayload, ValueSyncConfig,
};
use malachitebft_eth_types::{Genesis, PrivateKey, Validator, ValidatorSet};
use std::{path::PathBuf, time::Duration};
mod app;
mod config;
mod metrics;
mod node;
mod state;
mod store;
mod streaming;

#[derive(Parser)]
struct Args {
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[arg(long, global = true, default_value = "info")]
    log_level: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Testnet {
        #[arg(long, default_value_t = 4)]
        nodes: usize,
        #[arg(long, default_value = "multi-threaded:2")]
        runtime: String,
    },
    Start,
}
fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(&args.log_level)
        .init();
    let home = args.home.ok_or_else(|| eyre::eyre!("--home is required"))?;
    match args.command {
        Command::Testnet { nodes, runtime } => {
            ensure!(nodes == 4, "this PoC requires four equal-weight validators");
            ensure!(
                runtime == "multi-threaded:2",
                "this PoC uses two runtime threads per validator"
            );
            let keys: Vec<_> = (0..nodes)
                .map(|_| PrivateKey::generate(rand::rngs::OsRng))
                .collect();
            let genesis = Genesis {
                validator_set: ValidatorSet::new(
                    keys.iter().map(|key| Validator::new(key.public_key(), 1)),
                ),
            };
            for (i, key) in keys.iter().enumerate() {
                let dir = home.join(i.to_string()).join("config");
                ensure!(!dir.exists(), "refusing to overwrite {}", dir.display());
                std::fs::create_dir_all(&dir)?;
                let timeouts = malachitebft_config::TimeoutConfig::default();
                let config = config::Config {
                    moniker: format!("test-{i}"),
                    consensus: ConsensusConfig {
                        timeouts,
                        p2p: P2pConfig {
                            listen_addr: format!("/ip4/127.0.0.1/tcp/{}", 27000 + i).parse()?,
                            persistent_peers: (0..nodes)
                                .filter(|j| *j != i)
                                .map(|j| {
                                    format!("/ip4/127.0.0.1/tcp/{}", 27000 + j).parse().unwrap()
                                })
                                .collect(),
                            ..Default::default()
                        },
                        value_payload: ValuePayload::ProposalAndParts,
                        queue_capacity: 100,
                    },
                    value_sync: ValueSyncConfig {
                        status_update_interval: Duration::from_secs(1),
                        ..Default::default()
                    },
                    metrics: MetricsConfig {
                        enabled: true,
                        listen_addr: format!("127.0.0.1:{}", 29000 + i).parse()?,
                    },
                };
                std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config)?)?;
                std::fs::write(
                    dir.join("genesis.json"),
                    serde_json::to_vec_pretty(&genesis)?,
                )?;
                std::fs::write(
                    dir.join("priv_validator_key.json"),
                    serde_json::to_vec_pretty(key)?,
                )?;
            }
            Ok(())
        }
        Command::Start => {
            let dir = home.join("config");
            let config = toml::from_str(&std::fs::read_to_string(dir.join("config.toml"))?)?;
            let node = node::App {
                config,
                home_dir: home,
                genesis_file: dir.join("genesis.json"),
                private_key_file: dir.join("priv_validator_key.json"),
                start_height: None,
            };
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
                .block_on(node.run())
        }
    }
}
