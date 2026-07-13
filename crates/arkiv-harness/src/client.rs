//! A typed client over one node's `arkiv_*` + `eth_*` JSON-RPC.
//!
//! [`connect`] builds a signing client (can send `execute` transactions);
//! [`connect_reader`] a read-only one. The methods mirror the node's RPC surface —
//! entity reads, queries (tip and historical), counts, block timing — plus the tx
//! and block helpers the black-box tests need.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alloy_network::EthereumWallet;
use alloy_primitives::B256;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::TransactionReceipt;
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::{IEntityRegistry, Operation};
use arkiv_reth_executor::ARKIV_ADDRESS;

/// The gas + price every `execute` uses. Gas is set explicitly so the client
/// skips `eth_estimateGas` — which would fail up-front on a reverting tx, hiding
/// the revert that [`try_execute`](ArkivClient::try_execute) needs to observe.
const EXECUTE_GAS: u64 = 8_000_000;
const EXECUTE_GAS_PRICE: u128 = 1_000_000_000;

/// A typed RPC client bound to one node.
pub struct ArkivClient<P> {
    provider: P,
}

/// Connect a **signing** client (can send `execute` txs) to `http_url` as `signer`.
///
/// The `use<>` bound marks the returned client as capturing nothing — the provider
/// owns its parsed URL, it does not borrow `http_url` — so callers can pass a
/// temporary (`&node.http_url()`).
pub fn connect(
    http_url: &str,
    signer: PrivateKeySigner,
) -> ArkivClient<impl Provider + Clone + use<>> {
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(http_url.parse().expect("valid http url"));
    ArkivClient { provider }
}

/// Connect a **read-only** client (no wallet) to `http_url`.
pub fn connect_reader(http_url: &str) -> ArkivClient<impl Provider + Clone + use<>> {
    let provider = ProviderBuilder::new().connect_http(http_url.parse().expect("valid http url"));
    ArkivClient { provider }
}

impl<P: Provider> ArkivClient<P> {
    /// Wrap an existing provider.
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// The underlying provider, for `eth_*` calls the typed methods don't cover.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    // ── liveness / blocks ────────────────────────────────────────────────────

    /// Block until the node's RPC answers `eth_chainId`, or panic after `timeout`.
    pub async fn wait_ready(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            match self.provider.get_chain_id().await {
                Ok(_) => return,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "node RPC not ready in {timeout:?}: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    /// The chain id.
    pub async fn chain_id(&self) -> u64 {
        self.provider.get_chain_id().await.expect("chain id")
    }

    /// The current tip block number.
    pub async fn block_number(&self) -> u64 {
        self.provider
            .get_block_number()
            .await
            .expect("block number")
    }

    /// The hash of the current tip block — the head a CL driver points a follower at.
    pub async fn tip_hash(&self) -> B256 {
        self.block_hash("latest").await
    }

    /// The hash of block `number` (for asserting two chains agree at a fixed height).
    pub async fn block_hash_at(&self, number: u64) -> B256 {
        self.block_hash(&format!("0x{number:x}")).await
    }

    async fn block_hash(&self, block: &str) -> B256 {
        let response: serde_json::Value = self
            .provider
            .raw_request("eth_getBlockByNumber".into(), (block, false))
            .await
            .expect("eth_getBlockByNumber");
        response["hash"]
            .as_str()
            .unwrap_or_else(|| panic!("no block {block}"))
            .parse()
            .expect("block hash")
    }

    /// This node's enode URL (from `admin_nodeInfo`) — needs the `admin` RPC
    /// module. A follower dials it as a `--trusted-peers` entry.
    pub async fn enode(&self) -> String {
        let response: serde_json::Value = self
            .provider
            .raw_request("admin_nodeInfo".into(), ())
            .await
            .expect("admin_nodeInfo (is the `admin` module enabled?)");
        response["enode"]
            .as_str()
            .expect("enode string")
            .to_string()
    }

    /// Block until the chain reaches `target`, or panic after `timeout`.
    pub async fn wait_for_block(&self, target: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let current = self.block_number().await;
            if current >= target {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "block {target} not reached in {timeout:?} (at {current})",
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ── writes ───────────────────────────────────────────────────────────────

    /// Submit `ops` in one `execute` transaction and assert it succeeded,
    /// returning the receipt (for its logs).
    pub async fn execute(&self, ops: Vec<Operation>) -> TransactionReceipt {
        let receipt = self.send(ops).await;
        assert!(receipt.status(), "transaction reverted");
        receipt
    }

    /// Submit `ops` and return whether the tx succeeded (`true`) or reverted
    /// (`false`) — for ops expected to revert.
    pub async fn try_execute(&self, ops: Vec<Operation>) -> bool {
        self.send(ops).await.status()
    }

    async fn send(&self, ops: Vec<Operation>) -> TransactionReceipt {
        IEntityRegistry::new(ARKIV_ADDRESS, &self.provider)
            .execute(ops)
            .gas(EXECUTE_GAS)
            .gas_price(EXECUTE_GAS_PRICE)
            .send()
            .await
            .expect("send ops")
            .get_receipt()
            .await
            .expect("get receipt")
    }

    // ── arkiv_* reads ────────────────────────────────────────────────────────

    /// `arkiv_getEntity` at the tip.
    pub async fn get_entity(&self, key: B256) -> serde_json::Value {
        self.provider
            .raw_request("arkiv_getEntity".into(), (key,))
            .await
            .expect("arkiv_getEntity")
    }

    /// `arkiv_getEntity` as of a past block.
    pub async fn get_entity_at(&self, key: B256, block: u64) -> serde_json::Value {
        self.provider
            .raw_request("arkiv_getEntity".into(), (key, block))
            .await
            .expect("arkiv_getEntity (historical)")
    }

    /// `arkiv_query` at the tip.
    pub async fn query(
        &self,
        query: &str,
        page_size: u64,
        cursor: Option<u64>,
    ) -> serde_json::Value {
        let mut request = serde_json::json!({ "query": query, "pageSize": page_size });
        if let Some(cursor) = cursor {
            request["cursor"] = serde_json::json!(cursor);
        }
        self.query_raw(request)
            .await
            .unwrap_or_else(|e| panic!("arkiv_query {query:?}: {e}"))
    }

    /// `arkiv_query` as of a past block.
    pub async fn query_at(&self, query: &str, block: u64) -> serde_json::Value {
        let request = serde_json::json!({ "query": query, "pageSize": 100, "block": block });
        self.query_raw(request)
            .await
            .unwrap_or_else(|e| panic!("arkiv_query {query:?}@{block}: {e}"))
    }

    /// `arkiv_query` with a raw request object, surfacing RPC errors (for the
    /// error-contract assertions).
    pub async fn query_raw(&self, request: serde_json::Value) -> eyre::Result<serde_json::Value> {
        self.provider
            .raw_request("arkiv_query".into(), (request,))
            .await
            .map_err(|e| eyre::eyre!("{e}"))
    }

    /// `arkiv_getEntityCount` — optional query filter (default `$all`) and block
    /// (default the tip).
    pub async fn entity_count(&self, query: Option<&str>, block: Option<u64>) -> u64 {
        let mut request = serde_json::json!({});
        if let Some(query) = query {
            request["query"] = serde_json::json!(query);
        }
        if let Some(block) = block {
            request["block"] = serde_json::json!(block);
        }
        let response: serde_json::Value = self
            .provider
            .raw_request("arkiv_getEntityCount".into(), (request,))
            .await
            .expect("arkiv_getEntityCount");
        response["count"].as_u64().expect("count u64")
    }

    /// `arkiv_getBlockTiming`.
    pub async fn block_timing(&self) -> serde_json::Value {
        self.provider
            .raw_request("arkiv_getBlockTiming".into(), ())
            .await
            .expect("arkiv_getBlockTiming")
    }
}

/// The set of `key` strings in a query response — order-independent, since the
/// index returns newest-first but tests assert on membership.
pub fn result_keys(response: &serde_json::Value) -> BTreeSet<String> {
    response["entities"]
        .as_array()
        .expect("entities array")
        .iter()
        .map(|entity| entity["key"].as_str().expect("key string").to_string())
        .collect()
}
