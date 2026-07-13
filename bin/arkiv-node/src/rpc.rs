//! The `arkiv_*` JSON-RPC namespace.
//!
//! Methods are registered directly on a jsonrpsee [`RpcModule`] with native async
//! closures — no `#[rpc]` macro and no async-trait — and merged into reth's rpc
//! modules from `main`. Each read takes a fresh [`SnapshotAccountCode`] view of the
//! requested state — the tip by default, or a past block when the caller asks
//! (`block`), since the Arkiv index lives in ordinary reth state and a historical
//! state snapshot carries a historical index for free.
//!
//! Methods: `arkiv_getEntity`, `arkiv_query`, `arkiv_getEntityCount`,
//! `arkiv_getBlockTiming`.

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, hex};
use arkiv_interfaces::entity::{Attribute, Entity};
use arkiv_interfaces::query::{PageParams, Query, QueryStats};
use arkiv_interfaces::state::{AuxiliaryStore, EntityStore};
use arkiv_reth_auxstore::RethAuxStore;
use arkiv_reth_entitystore::{CodeBackend, RethEntityStore};
use jsonrpsee::RpcModule;
use jsonrpsee::types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};
use reth_storage_api::{BlockNumReader, HeaderProvider, StateProviderBox, StateProviderFactory};
use serde::{Deserialize, Serialize};

use crate::snapshot::SnapshotAccountCode;

/// Default page size when a `arkiv_query` request omits it.
const DEFAULT_PAGE_SIZE: u64 = 100;

/// Build the `arkiv_*` [`RpcModule`], ready to merge into reth's rpc modules.
///
/// `provider` hands out a fresh state snapshot per call (`StateProviderFactory`) and
/// answers block-header/number reads for `arkiv_getBlockTiming`.
pub fn arkiv_module<Provider>(provider: Provider) -> eyre::Result<RpcModule<()>>
where
    Provider:
        StateProviderFactory + HeaderProvider + BlockNumReader + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(());

    let p = provider.clone();
    module.register_async_method("arkiv_getEntity", move |params, _ctx, _ext| {
        let provider = p.clone();
        async move {
            // `[key]` at the tip, or `[key, block]` as of a past block.
            let mut seq = params.sequence();
            let key: B256 = seq
                .next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?;
            let block: Option<u64> = seq
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid block param: {e}")))?;
            // The read hits MDBX, so run it off the async runtime.
            tokio::task::spawn_blocking(move || read_entity(&provider, key, block))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
                .map_err(|e| internal_error(e.to_string()))
        }
    })?;

    let p = provider.clone();
    module.register_async_method("arkiv_query", move |params, _ctx, _ext| {
        let provider = p.clone();
        async move {
            let request: QueryRequest = params
                .one()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?;
            // The parse is pure, but the evaluation hits MDBX — run it off the
            // async runtime.
            tokio::task::spawn_blocking(move || run_query(&provider, request))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    module.register_async_method("arkiv_getEntityCount", move |params, _ctx, _ext| {
        let provider = p.clone();
        async move {
            // Optional `[{ query?, block? }]`; absent means "all, at the tip".
            let request: CountRequest = params
                .sequence()
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || entity_count(&provider, request))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider;
    module.register_async_method("arkiv_getBlockTiming", move |_params, _ctx, _ext| {
        let provider = p.clone();
        async move {
            tokio::task::spawn_blocking(move || block_timing(&provider))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    Ok(module)
}

/// A state snapshot as of `block` (its post-state), or the tip when `block` is
/// `None`. The Arkiv index rides in this same state, so a historical snapshot
/// answers historical reads and queries without any per-store history machinery.
fn resolve_state<Provider>(
    provider: &Provider,
    block: Option<u64>,
) -> eyre::Result<StateProviderBox>
where
    Provider: StateProviderFactory,
{
    match block {
        Some(number) => provider
            .history_by_block_number(number)
            .map_err(|e| eyre::eyre!("history_by_block_number({number}): {e:?}")),
        None => provider
            .latest()
            .map_err(|e| eyre::eyre!("latest state: {e:?}")),
    }
}

/// Read one entity by key, at the tip or as of `block`.
fn read_entity<Provider>(
    provider: &Provider,
    key: B256,
    block: Option<u64>,
) -> eyre::Result<Option<EntityView>>
where
    Provider: StateProviderFactory,
{
    let state = resolve_state(provider, block)?;
    let mut store = RethEntityStore::new(CodeBackend::new(SnapshotAccountCode::new(state)));
    let entity = store
        .get(key.0)
        .map_err(|e| eyre::eyre!("get entity: {e:?}"))?;
    Ok(entity.map(EntityView::from_entity))
}

/// Parse `request.query`, evaluate it against the index (at the tip or as of
/// `request.block`), and read the matched entities back.
///
/// One snapshot backs both stores: the index [`evaluate`](AuxiliaryStore::evaluate)
/// resolves the query to a page of keys, then the same snapshot is recovered
/// (`into_backend`) and reused to read those entities' full bytes — so the keys and
/// the entities are read from a single consistent state.
fn run_query<Provider>(
    provider: &Provider,
    request: QueryRequest,
) -> Result<QueryResultView, ErrorObjectOwned>
where
    Provider: StateProviderFactory,
{
    let query = arkiv_query::parse(&request.query)
        .map_err(|e| invalid_params(format!("invalid query: {e:?}")))?;
    let page = PageParams {
        page_size: request.page_size.unwrap_or(DEFAULT_PAGE_SIZE),
        cursor: request.cursor,
    };
    if page.page_size == 0 {
        return Err(invalid_params("pageSize must be greater than zero".into()));
    }

    let state = resolve_state(provider, request.block)
        .map_err(|e| internal_error(format!("state: {e:?}")))?;

    let mut index = RethAuxStore::new(SnapshotAccountCode::new(state));
    let matches = index
        .evaluate(&query, page)
        .map_err(|e| internal_error(format!("evaluate: {e:?}")))?;

    // Recover the same snapshot to read the matched entities' bytes.
    let mut store = RethEntityStore::new(CodeBackend::new(index.into_backend()));
    let mut entities = Vec::with_capacity(matches.keys.len());
    for key in &matches.keys {
        // A key in the index but missing from the entity store is a store
        // inconsistency, not a normal "no such entity" — surface it.
        let entity = store
            .get(*key)
            .map_err(|e| internal_error(format!("get entity: {e:?}")))?
            .ok_or_else(|| {
                internal_error(format!(
                    "indexed entity {} missing from store",
                    hex_prefixed(key)
                ))
            })?;
        entities.push(EntityView::from_entity(entity));
    }

    Ok(QueryResultView {
        entities,
        next_cursor: matches.next_cursor,
        stats: QueryStatsView::from_stats(&matches.stats),
    })
}

/// Count the entities matching `request.query` (default `$all`), at the tip or as
/// of `request.block`.
///
/// The count is the full match-set cardinality
/// ([`entities_scanned`](QueryStats::entities_scanned)), independent of paging, so a
/// one-key page is enough to read it.
fn entity_count<Provider>(
    provider: &Provider,
    request: CountRequest,
) -> Result<CountView, ErrorObjectOwned>
where
    Provider: StateProviderFactory,
{
    let query = match &request.query {
        Some(text) => {
            arkiv_query::parse(text).map_err(|e| invalid_params(format!("invalid query: {e:?}")))?
        }
        None => Query::All,
    };
    let state = resolve_state(provider, request.block)
        .map_err(|e| internal_error(format!("state: {e:?}")))?;
    let mut index = RethAuxStore::new(SnapshotAccountCode::new(state));
    let matches = index
        .evaluate(
            &query,
            PageParams {
                page_size: 1,
                cursor: None,
            },
        )
        .map_err(|e| internal_error(format!("evaluate: {e:?}")))?;
    Ok(CountView {
        count: matches.stats.entities_scanned,
    })
}

/// The chain tip's number, timestamp, and the gap to the previous block —
/// `arkiv_getBlockTiming`.
fn block_timing<Provider>(provider: &Provider) -> Result<BlockTimingView, ErrorObjectOwned>
where
    Provider: HeaderProvider + BlockNumReader,
{
    let current_block = provider
        .best_block_number()
        .map_err(|e| internal_error(format!("best_block_number: {e:?}")))?;
    let header = provider
        .header_by_number(current_block)
        .map_err(|e| internal_error(format!("header_by_number({current_block}): {e:?}")))?
        .ok_or_else(|| internal_error(format!("no header for block {current_block}")))?;
    let current_block_time = header.timestamp();

    // Genesis has no predecessor, so its inter-block duration is zero.
    let duration = if current_block == 0 {
        0
    } else {
        let prev = provider
            .header_by_number(current_block - 1)
            .map_err(|e| internal_error(format!("header_by_number({}): {e:?}", current_block - 1)))?
            .ok_or_else(|| internal_error(format!("no header for block {}", current_block - 1)))?;
        current_block_time.saturating_sub(prev.timestamp())
    };

    Ok(BlockTimingView {
        current_block,
        current_block_time,
        duration,
    })
}

/// The `arkiv_query` request: a query string, optional paging, and an optional
/// past block to evaluate against.
///
/// Sent as a single JSON object (the sole positional param): `{ "query": "...",
/// "pageSize": 100, "cursor": 42, "block": 128 }`. `pageSize` defaults to
/// [`DEFAULT_PAGE_SIZE`]; `cursor` is omitted on the first page and echoes a prior
/// response's `nextCursor` to continue; `block` defaults to the tip.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryRequest {
    pub query: String,
    #[serde(default)]
    pub page_size: Option<u64>,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub block: Option<u64>,
}

/// The `arkiv_getEntityCount` request: an optional query to filter by (default
/// `$all`) and an optional past block (default the tip).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub block: Option<u64>,
}

/// The `arkiv_query` response: one page of matched entities, a continuation
/// cursor, and the work the query did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResultView {
    pub entities: Vec<EntityView>,
    pub next_cursor: Option<u64>,
    pub stats: QueryStatsView,
}

/// The `arkiv_getEntityCount` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountView {
    pub count: u64,
}

/// The `arkiv_getBlockTiming` response.
///
/// Serialized in `snake_case` — the shape the `arkiv-cli` `block-timing` command
/// deserializes.
#[derive(Debug, Clone, Serialize)]
pub struct BlockTimingView {
    /// The chain tip's block number.
    pub current_block: u64,
    /// The tip block's timestamp (unix seconds).
    pub current_block_time: u64,
    /// Seconds between the tip and its predecessor (`0` at genesis).
    pub duration: u64,
}

/// The JSON shape of a query's [`QueryStats`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryStatsView {
    pub entities_scanned: u64,
    pub entities_returned: u64,
    pub index_lookups: u64,
    pub gas_used: u64,
    pub partial: bool,
}

impl QueryStatsView {
    fn from_stats(stats: &QueryStats) -> Self {
        Self {
            entities_scanned: stats.entities_scanned,
            entities_returned: stats.entities_returned,
            index_lookups: stats.index_lookups,
            gas_used: stats.gas_used,
            partial: stats.partial,
        }
    }
}

/// The JSON shape of an entity: byte fields as `0x`-hex, text fields as strings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityView {
    pub key: String,
    pub owner: String,
    pub creator: String,
    pub created_at_block: u64,
    pub last_modified_at_block: u64,
    pub expires_at: u64,
    pub content_type: String,
    pub payload: String,
    pub attributes: Vec<AttributeView>,
}

/// The JSON shape of one attribute.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttributeView {
    pub key: String,
    pub value_type: u8,
    pub value: String,
}

impl EntityView {
    fn from_entity(entity: Entity) -> Self {
        Self {
            key: hex_prefixed(&entity.key),
            owner: hex_prefixed(&entity.owner),
            creator: hex_prefixed(&entity.creator),
            created_at_block: entity.created_at_block,
            last_modified_at_block: entity.last_modified_at_block,
            expires_at: entity.expires_at,
            content_type: String::from_utf8_lossy(&entity.content_type).into_owned(),
            payload: hex_prefixed(&entity.payload),
            attributes: entity
                .attributes
                .into_iter()
                .map(AttributeView::from_attribute)
                .collect(),
        }
    }
}

impl AttributeView {
    fn from_attribute(attribute: Attribute) -> Self {
        Self {
            key: String::from_utf8_lossy(&attribute.key).into_owned(),
            value_type: attribute.value_type,
            value: hex_prefixed(&attribute.value),
        }
    }
}

fn hex_prefixed(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn internal_error(message: String) -> ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, message, None::<()>)
}

fn invalid_params(message: String) -> ErrorObjectOwned {
    ErrorObject::owned(INVALID_PARAMS_CODE, message, None::<()>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::ATTR_STRING;

    #[test]
    fn entity_view_projects_bytes_as_hex_and_text_as_strings() {
        let entity = Entity {
            key: [0x11; 32],
            owner: [0x22; 20],
            creator: [0x33; 20],
            created_at_block: 5,
            last_modified_at_block: 6,
            expires_at: 100,
            content_type: b"text/plain".to_vec(),
            payload: vec![0xDE, 0xAD],
            attributes: vec![Attribute {
                key: b"color".to_vec(),
                value_type: ATTR_STRING,
                value: b"blue".to_vec(),
            }],
        };
        let view = EntityView::from_entity(entity);
        assert_eq!(view.key, format!("0x{}", "11".repeat(32)));
        assert_eq!(view.owner, format!("0x{}", "22".repeat(20)));
        assert_eq!(view.content_type, "text/plain");
        assert_eq!(view.payload, "0xdead");
        assert_eq!(view.expires_at, 100);
        assert_eq!(view.attributes[0].key, "color");
        assert_eq!(view.attributes[0].value, "0x626c7565"); // "blue"
    }
}
