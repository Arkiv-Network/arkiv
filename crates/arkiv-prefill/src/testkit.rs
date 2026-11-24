//! What the writer's and the run's tests share: GolemDB as the node opens it,
//! and archive lines.

use std::sync::Arc;

use arkiv_reth_statemanager::HostStore;

pub(crate) const CHAIN: u64 = 7_738_577;
pub(crate) const ALICE: &str = "0x1111111111111111111111111111111111111111";
pub(crate) const BOB: &str = "0x2222222222222222222222222222222222222222";

/// GolemDB's config as the node opens it on the example genesis: its prices,
/// and Arkiv's host weights.
fn config() -> arkiv_golemdb::GolemDbConfig {
    let spec = arkiv_reth_chainspec::ArkivChainSpec::from_genesis(
        serde_json::from_str(include_str!("../../../genesis/example.json")).unwrap(),
    )
    .unwrap();
    arkiv_golemdb::GolemDbConfig {
        genesis: spec.golemdb().unwrap().clone(),
        host_schemas: vec![arkiv_reth_chainspec::host_metering_schema()],
        segments: vec![],
    }
}

/// That GolemDB, in memory.
pub(crate) fn store() -> HostStore {
    Arc::new(arkiv_golemdb::GolemDbStore::open_memory(&config()).unwrap())
}

/// That GolemDB, on disk in `directory`: opening it again finds what was
/// committed before.
pub(crate) fn store_at(directory: &std::path::Path) -> HostStore {
    Arc::new(arkiv_golemdb::GolemDbStore::open(directory, &config(), None).unwrap())
}

/// One line: an entity `key` (a byte, repeated) created by `creator`, owned
/// by `owner`, expiring at `expires_at`, with `extra` fields.
pub(crate) fn line(key: u8, creator: &str, owner: &str, expires_at: u64, extra: &str) -> String {
    format!(
        r#"{{"$key":"0x{}","$creator":"{creator}","$owner":"{owner}","$expiresAt":{expires_at}{extra}}}"#,
        format!("{key:02x}").repeat(32)
    )
}
