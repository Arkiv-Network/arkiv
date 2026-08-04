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
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, hex};
use arkiv_interfaces::entity::{Attribute, Entity};
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, PageParams, Query};
use arkiv_interfaces::state::{AuxiliaryStore, EntityStore};
use arkiv_reth_auxstore::RethAuxStore;
use arkiv_reth_entitystore::{CodeBackend, RethEntityStore};
use jsonrpsee::RpcModule;
use jsonrpsee::types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};
use reth_storage_api::{BlockNumReader, HeaderProvider, StateProviderBox, StateProviderFactory};
use serde::{Deserialize, Serialize};

use crate::snapshot::SnapshotAccountCode;
use crate::view::{
    EntityData, IncludeData, ResolvedIncludeData, entity_data_from, parse_cursor, ser_u64_hex,
};

/// Default page size when a `arkiv_query` request omits it.
const DEFAULT_PAGE_SIZE: u64 = 100;
/// `resultsPerPage` is clamped to this ceiling.
const MAX_PAGE_SIZE: u64 = 200;

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
        }
    })?;

    let p = provider.clone();
    module.register_async_method("arkiv_query", move |params, _ctx, _ext| {
        let provider = p.clone();
        async move {
            // Positional `[q]` or `[q, options]` — the SDK's wire format.
            let mut seq = params.sequence();
            let q: String = seq
                .next()
                .map_err(|e| invalid_params(format!("invalid query param: {e}")))?;
            let options: QueryOptions = seq
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid options param: {e}")))?
                .unwrap_or_default();
            // The parse is pure, but the evaluation hits MDBX — run it off the
            // async runtime.
            tokio::task::spawn_blocking(move || run_query(&provider, &q, &options))
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
/// `None`, plus the block number it answers for. The Arkiv index rides in this
/// same state, so a historical snapshot answers historical reads and queries
/// without any per-store history machinery.
fn resolve_state<Provider>(
    provider: &Provider,
    block: Option<u64>,
) -> Result<(StateProviderBox, u64), ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    snapshot_for(provider, block.map(BlockNumberOrTag::Number))
}

// ── BTL filtering ─────────────────────────────────────────────────────
//
// An entity whose BTL has run out is dead state, but it lingers in the entity
// store and the index until an explicit `expire` op prunes it. Every read path
// therefore has to apply the expiry rule itself, or it serves rows that should
// no longer exist. Both helpers below encode that one rule — the executor's
// check in `arkiv-reth-executor`'s `mutate` — in the two shapes the read paths
// need.

/// The rule: an entity is live at `block` while its expiry block is still ahead.
fn is_live(expires_at: BlockNumber, block: BlockNumber) -> bool {
    expires_at > block
}

/// The same rule as a query bound — `query AND $expiration > block`.
///
/// `$expiration` is an indexed built-in, so this filters inside the index rather
/// than after the fact: pages come back full instead of pocked with holes, and
/// the match-set cardinality behind `arkiv_getEntityCount` excludes dead rows.
fn live_at(query: Query, block: BlockNumber) -> Query {
    Query::And(
        Box::new(query),
        Box::new(Query::Gt {
            key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
            value: AnnotVal::u256_from_u64(block),
        }),
    )
}

/// Read one entity by key, at the tip or as of `block`. A past-BTL entity reads
/// as absent — see [`is_live`].
fn read_entity<Provider>(
    provider: &Provider,
    key: B256,
    block: Option<u64>,
) -> Result<Option<EntityView>, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let (state, block_number) = resolve_state(provider, block)?;
    let mut store = RethEntityStore::new(CodeBackend::new(SnapshotAccountCode::new(state)));
    let entity = store
        .get(key.0)
        .map_err(|e| internal_error(format!("get entity: {e:?}")))?;
    Ok(entity
        .filter(|e| is_live(e.expires_at, block_number))
        .map(EntityView::from_entity))
}

/// A state snapshot for `atBlock` plus the block number it answers for:
/// `None`/`"latest"` → the tip (and its number), a hex number → that block's
/// post-state. Other tags are rejected.
fn snapshot_for<Provider>(
    provider: &Provider,
    at_block: Option<BlockNumberOrTag>,
) -> Result<(StateProviderBox, u64), ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    match at_block {
        None | Some(BlockNumberOrTag::Latest) => {
            let n = provider
                .best_block_number()
                .map_err(|e| internal_error(format!("best_block_number: {e:?}")))?;
            let state = provider
                .latest()
                .map_err(|e| internal_error(format!("latest state: {e:?}")))?;
            Ok((state, n))
        }
        Some(BlockNumberOrTag::Number(n)) => {
            let state = provider
                .history_by_block_number(n)
                .map_err(|e| internal_error(format!("history_by_block_number({n}): {e:?}")))?;
            Ok((state, n))
        }
        Some(other) => Err(invalid_params(format!(
            "atBlock tag {other:?} not supported; use a hex block number or 'latest'"
        ))),
    }
}

/// Parse `q`, evaluate it against the index (at the tip or as of
/// `options.atBlock`), and read the matched entities back.
///
/// One snapshot backs both stores: the index [`evaluate`](AuxiliaryStore::evaluate)
/// resolves the query to a page of keys, then the same snapshot is recovered
/// (`into_backend`) and reused to read those entities' full bytes — so the keys and
/// the entities are read from a single consistent state.
///
/// The query is bounded to entities still live at that block — see [`live_at`].
fn run_query<Provider>(
    provider: &Provider,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let query =
        arkiv_query::parse(q).map_err(|e| invalid_params(format!("invalid query: {e:?}")))?;
    let page = PageParams {
        page_size: options
            .results_per_page
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
        cursor: parse_cursor(options.cursor.as_deref())
            .map_err(|e| invalid_params(e.to_string()))?,
    };

    let (state, block_number) = snapshot_for(provider, options.at_block)?;

    let mut index = RethAuxStore::new(SnapshotAccountCode::new(state));
    let matches = index
        .evaluate(&live_at(query, block_number), page)
        .map_err(|e| internal_error(format!("evaluate: {e:?}")))?;

    // Recover the same snapshot to read the matched entities' bytes.
    let mut store = RethEntityStore::new(CodeBackend::new(index.into_backend()));
    let inc = ResolvedIncludeData::from_options(options.include_data.as_ref());
    let mut data = Vec::with_capacity(matches.keys.len());
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
        data.push(entity_data_from(entity, &inc));
    }

    Ok(QueryResponse {
        data,
        block_number,
        cursor: matches.next_cursor.map(|c| format!("0x{c:x}")),
    })
}

/// Count the entities matching `request.query` (default `$all`), at the tip or as
/// of `request.block`.
///
/// The count is the full match-set cardinality
/// ([`entities_scanned`](QueryStats::entities_scanned)), independent of paging, so a
/// one-key page is enough to read it. Past-BTL entities are excluded — see
/// [`live_at`].
fn entity_count<Provider>(
    provider: &Provider,
    request: CountRequest,
) -> Result<u64, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let query = match &request.query {
        Some(text) => {
            arkiv_query::parse(text).map_err(|e| invalid_params(format!("invalid query: {e:?}")))?
        }
        None => Query::All,
    };
    let (state, block_number) = resolve_state(provider, request.block)?;
    let mut index = RethAuxStore::new(SnapshotAccountCode::new(state));
    let matches = index
        .evaluate(
            &live_at(query, block_number),
            PageParams {
                page_size: 1,
                cursor: None,
            },
        )
        .map_err(|e| internal_error(format!("evaluate: {e:?}")))?;
    Ok(matches.stats.entities_scanned)
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

/// The `arkiv_query` options — the SDK's second positional param.
///
/// The full call is `["<query>", { "atBlock": "0x1a", "resultsPerPage": 100,
/// "cursor": "0x2a", "includeData": { ... } }]`; the options object and each of
/// its fields are optional. `resultsPerPage` defaults to [`DEFAULT_PAGE_SIZE`]
/// and is clamped to `[1, MAX_PAGE_SIZE]`; `cursor` is omitted on the first page
/// and echoes a prior response's `cursor` to continue; `atBlock` defaults to the
/// tip.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryOptions {
    /// Block to evaluate against. `None` / `"latest"` reads head state.
    /// Hex number (`"0x1a"`) reads historical state. Other tags rejected.
    pub at_block: Option<BlockNumberOrTag>,
    /// Page size, as a JSON number or hex string; clamped to `[1, MAX_PAGE_SIZE]`.
    #[serde(default, deserialize_with = "crate::view::de_u64_flexible")]
    pub results_per_page: Option<u64>,
    /// Opaque cursor from the previous page response.
    pub cursor: Option<String>,
    /// Per-field projection. `None` → include all fields.
    pub include_data: Option<IncludeData>,
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

/// The `arkiv_query` response: one page of matched entities, the block the
/// query evaluated against (hex), and a continuation cursor (hex, absent on
/// the last page).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<EntityData>,
    #[serde(serialize_with = "ser_u64_hex")]
    pub block_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
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

/// The JSON shape of an entity: byte fields as `0x`-hex, text fields as strings.
/// Used by `arkiv_getEntity` only — `arkiv_query` answers with the SDK's
/// [`EntityData`] shape.
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
            value_type: attribute.value.type_id(),
            value: hex_prefixed(&attribute.value.encode()),
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
    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::entity::CreationFlags;

    #[test]
    fn entity_view_projects_bytes_as_hex_and_text_as_strings() {
        let entity = Entity {
            key: [0x11; 32],
            owner: [0x22; 20],
            creator: [0x33; 20],
            created_at_block: 5,
            last_modified_at_block: 6,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: vec![0xDE, 0xAD],
            attributes: vec![Attribute::new(
                b"color".to_vec(),
                AttributeValue::Str("blue".into()),
            )],
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
