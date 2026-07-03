//! `arkiv_*` JSON-RPC namespace.
//!
//! Thin adapter: owns the JSON-RPC plumbing, wire-format types, and the
//! [`StateProvider`] snapshot selection. Entity view logic (field projection,
//! attribute formatting, cursor parsing) lives in [`arkiv_db_engine::view`].

use alloy_eips::BlockNumberOrTag;
use arkiv_db_engine::{
    all_entities,
    query::{Page, PageParams, execute},
    view::{EntityData, IncludeData, ResolvedIncludeData, entity_data_from, parse_cursor, ser_u64_hex},
};
use async_trait::async_trait;
use eyre::Result;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::error::{ErrorObject, ErrorObjectOwned, INTERNAL_ERROR_CODE},
};
use reth_storage_api::{
    BlockNumReader, DatabaseProviderFactory, DBProvider, HeaderProvider, StateProviderFactory,
};
use serde::{Deserialize, Serialize};

use crate::state_adapter::ReadOnlyStateAdapter;

const DEFAULT_PAGE_SIZE: u64 = 100;
const MAX_PAGE_SIZE: u64 = 200;

// ── RPC trait ─────────────────────────────────────────────────────────

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    /// Evaluate a query and return matching entities, paginated descending
    /// by entity ID (newest first). Pass the returned `cursor` back as
    /// `options.cursor` to fetch the next page.
    #[method(name = "query")]
    async fn query(&self, q: String, options: Option<QueryOptions>) -> RpcResult<QueryResponse>;

    /// Number of live entities at the head block.
    #[method(name = "getEntityCount")]
    async fn get_entity_count(&self) -> RpcResult<u64>;

    /// Head block number, timestamp, and block duration in seconds.
    #[method(name = "getBlockTiming")]
    async fn get_block_timing(&self) -> RpcResult<BlockTiming>;
}

// ── Request / response types ──────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryOptions {
    /// Block to evaluate against. `None` / `"latest"` reads head state.
    /// Hex number (`"0x1a"`) reads historical state. Other tags rejected.
    pub at_block: Option<BlockNumberOrTag>,
    /// Page size; clamped to `[1, MAX_PAGE_SIZE]`.
    #[serde(default, deserialize_with = "de_u64_flexible_opt")]
    pub results_per_page: Option<u64>,
    /// Opaque cursor from the previous page response.
    pub cursor: Option<String>,
    /// Per-field projection. `None` → include all fields.
    pub include_data: Option<IncludeData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<EntityData>,
    #[serde(serialize_with = "ser_u64_hex", deserialize_with = "de_u64_hex")]
    pub block_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BlockTiming {
    pub current_block: u64,
    pub current_block_time: u64,
    pub duration: u64,
}

// ── Server struct ─────────────────────────────────────────────────────

pub struct ArkivRpc<P> {
    provider: P,
}

impl<P> ArkivRpc<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }
}

// ── Server impl ───────────────────────────────────────────────────────

#[async_trait]
impl<P> ArkivApiServer for ArkivRpc<P>
where
    P: StateProviderFactory
        + DatabaseProviderFactory
        + HeaderProvider
        + BlockNumReader
        + Clone
        + Send
        + Sync
        + 'static,
    P::Provider: DBProvider,
    <P::Provider as DBProvider>::Tx: reth_db_api::transaction::DbTx,
{
    async fn query(&self, q: String, options: Option<QueryOptions>) -> RpcResult<QueryResponse> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || run_query(&provider, &q, &options.unwrap_or_default()))
            .await
            .map_err(|e| internal_err(format!("join: {e}")))?
            .map_err(|e| internal_err(e.to_string()))
    }

    async fn get_entity_count(&self) -> RpcResult<u64> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let db_provider = provider
                .database_provider_ro()
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            let state = provider.latest().map_err(|e| eyre::eyre!("{e:?}"))?;
            let mut adapter = ReadOnlyStateAdapter::new(state, db_provider);
            Ok(all_entities(&mut adapter)?.len())
        })
        .await
        .map_err(|e| internal_err(format!("join: {e}")))?
        .map_err(|e| internal_err(e.to_string()))
    }

    async fn get_block_timing(&self) -> RpcResult<BlockTiming> {
        use alloy_consensus::BlockHeader;
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || -> Result<BlockTiming> {
            let current_block = provider
                .best_block_number()
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            let head = provider
                .header_by_number(current_block)
                .map_err(|e| eyre::eyre!("{e:?}"))?
                .ok_or_else(|| eyre::eyre!("head header missing for block {current_block}"))?;
            let current_block_time = head.timestamp();
            let duration = if current_block == 0 {
                0
            } else {
                let parent = provider
                    .header_by_number(current_block - 1)
                    .map_err(|e| eyre::eyre!("{e:?}"))?
                    .ok_or_else(|| {
                        eyre::eyre!("parent header missing for block {}", current_block - 1)
                    })?;
                current_block_time.saturating_sub(parent.timestamp())
            };
            Ok(BlockTiming { current_block, current_block_time, duration })
        })
        .await
        .map_err(|e| internal_err(format!("join: {e}")))?
        .map_err(|e| internal_err(e.to_string()))
    }
}

// ── Query execution ───────────────────────────────────────────────────

fn run_query<P>(provider: &P, q: &str, options: &QueryOptions) -> Result<QueryResponse>
where
    P: StateProviderFactory + DatabaseProviderFactory + HeaderProvider,
    P::Provider: DBProvider,
    <P::Provider as DBProvider>::Tx: reth_db_api::transaction::DbTx,
{
    let (state, block_number) = snapshot_for(provider, options.at_block)?;
    let db_provider = provider.database_provider_ro().map_err(|e| eyre::eyre!("{e:?}"))?;
    let mut adapter = ReadOnlyStateAdapter::new(state, db_provider);

    let params = PageParams {
        page_size: options
            .results_per_page
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
        cursor: parse_cursor(options.cursor.as_deref())?,
    };
    let Page { entries, next_cursor } = execute(&mut adapter, q, params)?;
    let inc = ResolvedIncludeData::from_options(options.include_data.as_ref());

    Ok(QueryResponse {
        data: entries.into_iter().map(|e| entity_data_from(e, &inc)).collect(),
        block_number,
        cursor: next_cursor.map(|id| format!("0x{id:x}")),
    })
}

fn snapshot_for<P: StateProviderFactory + HeaderProvider>(
    provider: &P,
    at_block: Option<BlockNumberOrTag>,
) -> Result<(reth_storage_api::StateProviderBox, u64)> {
    match at_block {
        None | Some(BlockNumberOrTag::Latest) => {
            let n = provider.best_block_number().map_err(|e| eyre::eyre!("{e:?}"))?;
            Ok((provider.latest().map_err(|e| eyre::eyre!("{e:?}"))?, n))
        }
        Some(BlockNumberOrTag::Number(n)) => {
            let state = provider
                .history_by_block_number(n)
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            Ok((state, n))
        }
        Some(other) => {
            eyre::bail!("atBlock tag {other:?} not supported; use a hex block number or 'latest'")
        }
    }
}

// ── Serde helpers ─────────────────────────────────────────────────────

fn de_u64_hex<'de, D>(de: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Hex(String),
    }
    match Either::deserialize(de)? {
        Either::Num(n) => Ok(n),
        Either::Hex(s) => {
            let stripped =
                s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(&s);
            u64::from_str_radix(stripped, 16)
                .map_err(|e| D::Error::custom(format!("invalid hex u64 {s:?}: {e}")))
        }
    }
}

fn de_u64_flexible_opt<'de, D>(de: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Hex(String),
    }
    let opt: Option<Either> = Option::deserialize(de)?;
    match opt {
        None => Ok(None),
        Some(Either::Num(n)) => Ok(Some(n)),
        Some(Either::Hex(s)) => {
            let stripped =
                s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(&s);
            u64::from_str_radix(stripped, 16)
                .map(Some)
                .map_err(|e| D::Error::custom(format!("invalid hex u64 {s:?}: {e}")))
        }
    }
}

fn internal_err(msg: String) -> ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, msg, None::<()>)
}
