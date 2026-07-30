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

/// How many times [`send`](ArkivClient::send) submits one batch of ops, and the
/// pause between attempts. A submission the node rejects for a stale nonce
/// consumed nothing, so re-submitting runs the nonce filler against the chain as
/// it now stands.
const SUBMIT_ATTEMPTS: u32 = 3;
const SUBMIT_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// How long the provider's watcher waits for the transaction to land before the
/// client takes over and polls `eth_getTransactionReceipt` itself, at
/// [`RECEIPT_POLL_INTERVAL`].
const RECEIPT_WATCH_TIMEOUT: Duration = Duration::from_secs(15);
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The whole budget for a receipt, measured from a successful submission. It
/// covers the watcher and the polling that follows it.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

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
        assert!(
            receipt.status(),
            "transaction {} reverted in block {:?}",
            receipt.transaction_hash,
            receipt.block_number,
        );
        receipt
    }

    /// Submit `ops` and return whether the tx succeeded (`true`) or reverted
    /// (`false`) — for ops expected to revert.
    pub async fn try_execute(&self, ops: Vec<Operation>) -> bool {
        self.send(ops).await.status()
    }

    /// Submit `ops` as one `execute` transaction and wait for its receipt,
    /// panicking if either stage fails.
    ///
    /// Every failure names the transaction hash, once one exists, and the chain
    /// height at the time, which separates a rejected transaction from a chain
    /// that stopped producing blocks.
    async fn send(&self, ops: Vec<Operation>) -> TransactionReceipt {
        let registry = IEntityRegistry::new(ARKIV_ADDRESS, &self.provider);
        let mut attempt = 1;
        let pending = loop {
            let call = registry
                .execute(ops.clone())
                .gas(EXECUTE_GAS)
                .gas_price(EXECUTE_GAS_PRICE);
            match call.send().await {
                Ok(pending) => break pending,
                Err(e) if is_nonce_too_low(&e) && attempt < SUBMIT_ATTEMPTS => {
                    attempt += 1;
                    tokio::time::sleep(SUBMIT_RETRY_BACKOFF).await;
                }
                Err(e) => panic!(
                    "send ops failed on attempt {attempt}/{SUBMIT_ATTEMPTS} at block {}: {e}",
                    self.height().await
                ),
            }
        };

        // The nonce is spent from here on, so the transaction is never re-sent:
        // the watcher and the polling that follows it both ask only about this
        // one hash.
        let tx_hash = *pending.tx_hash();
        let submitted_at = self.height().await;
        let deadline = Instant::now() + RECEIPT_TIMEOUT;
        let watch_error = match pending
            .with_timeout(Some(RECEIPT_WATCH_TIMEOUT))
            .get_receipt()
            .await
        {
            Ok(receipt) => return receipt,
            Err(e) => e,
        };

        loop {
            if let Some(receipt) = self
                .provider
                .get_transaction_receipt(tx_hash)
                .await
                .ok()
                .flatten()
            {
                return receipt;
            }
            assert!(
                Instant::now() < deadline,
                "no receipt for {tx_hash} in {RECEIPT_TIMEOUT:?}: submitted at block \
                 {submitted_at}, chain now at block {}, tx in pool: {} (watcher: {watch_error})",
                self.height().await,
                self.pool_state(tx_hash).await,
            );
            tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
        }
    }

    /// The tip block number for a failure message, or why it is unavailable —
    /// an unreachable node is itself part of the diagnosis.
    async fn height(&self) -> String {
        match self.provider.get_block_number().await {
            Ok(number) => number.to_string(),
            Err(e) => format!("<unknown: {e}>"),
        }
    }

    /// Where the node's transaction pool holds `tx_hash` — `pending` (ready to
    /// be mined), `queued` (parked behind a nonce gap) or `absent` (dropped, or
    /// already mined) — followed by the pool's totals. Reports an RPC error in
    /// place of whichever half it could not read.
    async fn pool_state(&self, tx_hash: B256) -> String {
        let status = match self
            .provider
            .raw_request::<_, serde_json::Value>("txpool_status".into(), ())
            .await
        {
            Ok(status) => format!(
                "{} pending / {} queued",
                quantity(&status["pending"]),
                quantity(&status["queued"])
            ),
            Err(e) => format!("<unknown: {e}>"),
        };
        let content = match self
            .provider
            .raw_request::<_, serde_json::Value>("txpool_content".into(), ())
            .await
        {
            Ok(content) => content,
            Err(e) => return format!("<unknown: {e}> (status: {status})"),
        };

        let wanted = tx_hash.to_string();
        let holds = |group: &serde_json::Value| {
            group
                .as_object()
                .into_iter()
                .flat_map(|by_sender| by_sender.values())
                .filter_map(serde_json::Value::as_object)
                .flat_map(|by_nonce| by_nonce.values())
                .any(|tx| tx["hash"].as_str() == Some(wanted.as_str()))
        };
        let group = if holds(&content["pending"]) {
            "pending"
        } else if holds(&content["queued"]) {
            "queued"
        } else {
            "absent"
        };
        format!("{group} (status: {status})")
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

/// A JSON-RPC quantity rendered for a message: the node's hex string without
/// its JSON quotes, or the raw JSON if it answered with something else.
fn quantity(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

/// Whether a submission error is the node rejecting the transaction's nonce as
/// already used — the one submission failure a retry can clear.
fn is_nonce_too_low(error: &impl std::fmt::Display) -> bool {
    error.to_string().to_lowercase().contains("nonce too low")
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
