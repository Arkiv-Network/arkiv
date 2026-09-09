//! The Application (or Node) definition. The Node trait implements the Consensus context and the
//! cryptographic library used for signing.

use std::path::PathBuf;
use std::str::FromStr;

use async_trait::async_trait;
use color_eyre::eyre;
use malachitebft_app_channel::app::events::{RxEvent, TxEvent};
use malachitebft_app_channel::app::node::NodeHandle;
use malachitebft_eth_engine::engine::Engine;
use malachitebft_eth_engine::engine_rpc::EngineRPC;
use malachitebft_eth_engine::ethereum_rpc::EthereumRPC;

use crate::config::Config;
use malachitebft_app_channel::app::node::{EngineHandle, Node};
use malachitebft_app_channel::app::types::Keypair;

// Use the same types used for integration tests.
// A real application would use its own types and context instead.
use crate::metrics;
use malachitebft_eth_types::codec::proto::ProtobufCodec;
use malachitebft_eth_types::{
    Address, Ed25519Provider, Genesis, Height, PrivateKey, PublicKey, TestContext,
};
use tokio::task::JoinHandle;
use url::Url;

use crate::state::State;
use crate::store::Store;

/// Main application struct implementing the consensus node functionality
#[derive(Clone)]
pub struct App {
    pub config: Config,
    pub home_dir: PathBuf,
    pub genesis_file: PathBuf,
    pub private_key_file: PathBuf,
    pub start_height: Option<Height>,
}

pub struct Handle {
    pub app: JoinHandle<()>,
    pub engine: EngineHandle,
    pub tx_event: TxEvent<TestContext>,
}

#[async_trait]
impl NodeHandle<TestContext> for Handle {
    fn subscribe(&self) -> RxEvent<TestContext> {
        self.tx_event.subscribe()
    }

    async fn kill(&self, _reason: Option<String>) -> eyre::Result<()> {
        self.engine.actor.kill_and_wait(None).await?;
        self.app.abort();
        self.engine.handle.abort();
        Ok(())
    }
}

#[async_trait]
impl Node for App {
    type Context = TestContext;
    type Config = Config;

    fn load_config(&self) -> eyre::Result<Config> {
        Ok(self.config.clone())
    }
    type Genesis = Genesis;
    type PrivateKeyFile = PrivateKey;
    type SigningProvider = Ed25519Provider;
    type NodeHandle = Handle;

    fn get_home_dir(&self) -> PathBuf {
        self.home_dir.to_owned()
    }

    fn get_signing_provider(&self, private_key: PrivateKey) -> Self::SigningProvider {
        Ed25519Provider::new(private_key)
    }

    fn get_address(&self, pk: &PublicKey) -> Address {
        Address::from_public_key(pk)
    }

    fn get_public_key(&self, pk: &PrivateKey) -> PublicKey {
        pk.public_key()
    }

    fn get_keypair(&self, pk: PrivateKey) -> Keypair {
        Keypair::ed25519_from_bytes(pk.inner().to_bytes()).unwrap()
    }

    fn load_private_key(&self, file: Self::PrivateKeyFile) -> PrivateKey {
        file
    }

    fn load_private_key_file(&self) -> eyre::Result<Self::PrivateKeyFile> {
        let private_key = std::fs::read_to_string(&self.private_key_file)?;
        serde_json::from_str(&private_key).map_err(|e| e.into())
    }

    fn load_genesis(&self) -> eyre::Result<Self::Genesis> {
        let genesis = std::fs::read_to_string(&self.genesis_file)?;
        serde_json::from_str(&genesis).map_err(|e| e.into())
    }

    async fn start(&self) -> eyre::Result<Handle> {
        let span = tracing::error_span!("node", moniker = %self.config.moniker);
        let _enter = span.enter();

        let private_key_file = self.load_private_key_file()?;
        let private_key = self.load_private_key(private_key_file);
        let public_key = self.get_public_key(&private_key);
        let address = self.get_address(&public_key);
        let signing_provider = self.get_signing_provider(private_key);
        let ctx = TestContext::new();

        let genesis = self.load_genesis()?;
        let initial_validator_set = genesis.validator_set.clone();

        let codec = ProtobufCodec;

        let (mut channels, engine_handle) = malachitebft_app_channel::start_engine(
            ctx,
            self.clone(),
            self.config.clone(),
            codec,
            codec,
            self.start_height,
            initial_validator_set,
        )
        .await?;

        let tx_event = channels.events.clone();

        if self.config.metrics.enabled {
            tokio::spawn(metrics::serve(self.config.metrics.listen_addr));
        }

        let store = Store::open(self.get_home_dir().join("store.db"))?;
        let start_height = self.start_height.unwrap_or_default();
        let mut state = State::new(genesis, ctx, signing_provider, address, start_height, store);

        let engine = Engine::new(
            EngineRPC::new(
                Url::parse(&std::env::var("ARKIV_ENGINE_URL")?)?,
                PathBuf::from_str(&std::env::var("ARKIV_JWT_PATH")?)?.as_path(),
            )?,
            EthereumRPC::new(Url::parse(&std::env::var("ARKIV_ETH_URL")?)?)?,
        );

        let app_handle = tokio::spawn(async move {
            if let Err(e) = crate::app::run(&mut state, &mut channels, engine).await {
                tracing::error!(%e, "Application error");
                std::process::exit(1);
            }
        });

        Ok(Handle {
            app: app_handle,
            engine: engine_handle,
            tx_event,
        })
    }

    async fn run(self) -> eyre::Result<()> {
        let handles = self.start().await?;
        handles.app.await.map_err(Into::into)
    }
}
