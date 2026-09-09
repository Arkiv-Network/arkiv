//! Application configuration implementing the published Malachite NodeConfig API.
use malachitebft_config::{ConsensusConfig, MetricsConfig, ValueSyncConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub moniker: String,
    pub consensus: ConsensusConfig,
    pub value_sync: ValueSyncConfig,
    pub metrics: MetricsConfig,
}
impl malachitebft_app::node::NodeConfig for Config {
    fn moniker(&self) -> &str {
        &self.moniker
    }
    fn consensus(&self) -> &ConsensusConfig {
        &self.consensus
    }
    fn value_sync(&self) -> &ValueSyncConfig {
        &self.value_sync
    }
}
