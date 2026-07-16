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

    /// `arkiv_query` at the tip. `cursor` is the hex string echoed by a prior
    /// response's `cursor` field.
    pub async fn query(
        &self,
        query: &str,
        page_size: u64,
        cursor: Option<&str>,
    ) -> serde_json::Value {
        let mut options = serde_json::json!({ "resultsPerPage": page_size });
        if let Some(cursor) = cursor {
            options["cursor"] = serde_json::json!(cursor);
        }
        self.query_raw(query, options)
            .await
            .unwrap_or_else(|e| panic!("arkiv_query {query:?}: {e}"))
    }

    /// `arkiv_query` as of a past block.
    pub async fn query_at(&self, query: &str, block: u64) -> serde_json::Value {
        let options = serde_json::json!({ "atBlock": format!("0x{block:x}") });
        self.query_raw(query, options)
            .await
            .unwrap_or_else(|e| panic!("arkiv_query {query:?}@{block}: {e}"))
    }

    /// `arkiv_query` with raw positional params (`[q, options]` — the SDK's wire
    /// shape), surfacing RPC errors (for the error-contract assertions).
    pub async fn query_raw(
        &self,
        query: &str,
        options: serde_json::Value,
    ) -> eyre::Result<serde_json::Value> {
        self.provider
            .raw_request("arkiv_query".into(), (query, options))
            .await
            .map_err(|e| eyre::eyre!("{e}"))
    }

    /// `arkiv_getEntityCount` — optional query filter (default `$all`) and block
    /// (default the tip). Answers a bare number.
    pub async fn entity_count(&self, query: Option<&str>, block: Option<u64>) -> u64 {
        let mut request = serde_json::json!({});
        if let Some(query) = query {
            request["query"] = serde_json::json!(query);
        }
        if let Some(block) = block {
            request["block"] = serde_json::json!(block);
        }
        self.provider
            .raw_request::<_, u64>("arkiv_getEntityCount".into(), (request,))
            .await
            .expect("arkiv_getEntityCount")
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
    response["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|entity| entity["key"].as_str().expect("key string").to_string())
        .collect()
}
