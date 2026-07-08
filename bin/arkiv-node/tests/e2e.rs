//! End-to-end test: spawn `arkiv-node` in dev mode and prove the whole loop —
//! a client sends a `create` transaction, and reads the entity back over
//! `arkiv_getEntity`.
//!
//! This is black-box: it launches the actual node binary and drives it with alloy,
//! exercising the wired executor (decode → apply → commit), the minting nonce, and
//! the RPC read path against real reth state. It is heavier than the unit tests (it
//! boots a node), but it's the only thing that proves the modules compose live.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use alloy_network::EthereumWallet;
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{IEntityRegistry, Mime128, Operation};
use arkiv_reth_executor::{ARKIV_ADDRESS, derive_entity_key};

/// Test-mnemonic account #0 — the account reth's `--dev` mode pre-funds.
const DEV_PRIVATE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// reth `--dev` chain id.
const CHAIN_ID: u64 = 1337;

/// A running `arkiv-node --dev`, killed and cleaned up on drop.
struct DevNode {
    child: Child,
    port: u16,
    datadir: std::path::PathBuf,
}

impl DevNode {
    fn spawn() -> Self {
        // A per-process port/datadir so parallel test binaries don't collide.
        let port = 18000 + (std::process::id() % 2000) as u16;
        let datadir = std::env::temp_dir().join(format!("arkiv-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&datadir);

        // `arkiv` is intentionally NOT in --http.api: reth rejects unknown modules,
        // and the namespace is merged onto the http transport by extend_rpc_modules.
        let child = Command::new(env!("CARGO_BIN_EXE_arkiv-node"))
            .args([
                "node",
                "--dev",
                "--http",
                "--http.addr",
                "127.0.0.1",
                "--http.port",
                &port.to_string(),
                "--http.api",
                "eth,net,web3,txpool",
                "--dev.block-time",
                "250ms",
                "--datadir",
                datadir.to_str().unwrap(),
                "--disable-discovery",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn arkiv-node binary");

        Self {
            child,
            port,
            datadir,
        }
    }

    fn http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for DevNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

fn text_plain_mime() -> Mime128 {
    Mime128::encode("text/plain").expect("valid mime")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_then_get_entity_over_a_live_node() {
    let node = DevNode::spawn();

    let signer: PrivateKeySigner = DEV_PRIVATE_KEY.parse().unwrap();
    let caller = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(node.http_url().parse().unwrap());

    // Wait for the node's RPC to come up (debug reth takes a few seconds).
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match provider.get_chain_id().await {
            Ok(_) => break,
            Err(e) => {
                assert!(Instant::now() < deadline, "node RPC not ready in 90s: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    assert_eq!(provider.get_chain_id().await.unwrap(), CHAIN_ID);

    // 1) Send a `create` transaction — exactly what an SDK client does.
    let registry = IEntityRegistry::new(ARKIV_ADDRESS, &provider);
    let op = Operation::create(100, Bytes::from_static(b"hello"), text_plain_mime(), vec![]);
    let receipt = registry
        .execute(vec![op])
        .gas(3_000_000)
        .gas_price(1_000_000_000)
        .send()
        .await
        .expect("send create tx")
        .get_receipt()
        .await
        .expect("get receipt");
    assert!(receipt.status(), "create transaction reverted");

    // 2) The key the node minted for this caller's first create (minting nonce 0),
    //    derived independently — the test never learns it from the node.
    let key = B256::from(derive_entity_key(CHAIN_ID, &caller.into_array(), 0));

    // 3) Read it back over arkiv_getEntity and check the projection.
    let entity: serde_json::Value = provider
        .raw_request("arkiv_getEntity".into(), (key,))
        .await
        .expect("arkiv_getEntity");
    assert!(!entity.is_null(), "entity should exist after create");
    assert_eq!(entity["key"].as_str(), Some(format!("{key:#x}").as_str()));
    assert_eq!(
        entity["owner"].as_str(),
        Some(format!("{caller:#x}").as_str())
    );
    assert_eq!(
        entity["creator"].as_str(),
        Some(format!("{caller:#x}").as_str())
    );
    assert_eq!(entity["contentType"], "text/plain");
    assert_eq!(entity["payload"], "0x68656c6c6f"); // "hello"
    let created = entity["createdAtBlock"].as_u64().unwrap();
    assert_eq!(entity["expiresAt"].as_u64().unwrap(), created + 100); // btl resolved

    // 4) A key that was never created reads back as null.
    let missing: serde_json::Value = provider
        .raw_request("arkiv_getEntity".into(), (B256::repeat_byte(0xAB),))
        .await
        .expect("arkiv_getEntity (missing)");
    assert!(missing.is_null(), "a nonexistent key should read null");
}
