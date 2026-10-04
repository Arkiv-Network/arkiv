//! The `arkiv_*` JSON-RPC namespace, served over reth state.
//!
//! The read-path counterpart to `arkiv-reth-executor`: [`arkiv_module`] builds a
//! jsonrpsee [`RpcModule`] the binary merges into reth's rpc modules, answering
//! `arkiv_getEntity`, `arkiv_query`, `arkiv_getEntityCount` and
//! `arkiv_getBlockTiming`.
//!
//! Reads are answered from the GolemDB store, at one commit — see
//! [`store_reads`]. An entity is a record, so a read is a point `get`; a query
//! lowers to a store filter and is one `query` call, which also keeps a page
//! internally consistent because a commit is immutable.
//!
//! Wire shapes are the client's too and live in [`arkiv_rpc_types`]; this crate
//! holds only what a server decides.

pub mod cursor;
pub mod error;
pub mod store_reads;

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, Query};
use arkiv_interfaces::store::Store;
use arkiv_reth_statemanager::HostStore;
use arkiv_rpc_types::entity::{EntityData, Projection, entity_data_from};
use arkiv_rpc_types::method::{BlockTimingView, CountRequest, QueryOptions, QueryResponse};
use jsonrpsee::RpcModule;
use jsonrpsee::types::ErrorObjectOwned;
use reth_storage_api::{BlockNumReader, HeaderProvider, StateProviderFactory};

use crate::error::{block_unavailable, cursor_error, internal_error, invalid_params, query_error};

/// Build the `arkiv_*` [`RpcModule`], ready to merge into reth's rpc modules.
///
/// `provider` hands out a fresh state snapshot per call (`StateProviderFactory`) and
/// answers block-header/number reads for `arkiv_getBlockTiming`.
pub fn arkiv_module<Provider>(provider: Provider, store: HostStore) -> eyre::Result<RpcModule<()>>
where
    Provider:
        StateProviderFactory + HeaderProvider + BlockNumReader + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(());

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_getEntity", move |params, _ctx, _ext| {
        let provider = p.clone();
        let store = st.clone();
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
            tokio::task::spawn_blocking(move || read_entity(&provider, &store, key, block))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_debugEntityExists", move |params, _ctx, _ext| {
        let provider = p.clone();
        let store = st.clone();
        async move {
            let mut seq = params.sequence();
            let key: B256 = seq
                .next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?;
            let block: Option<u64> = seq
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid block param: {e}")))?;
            tokio::task::spawn_blocking(move || {
                entity_exists_unfiltered(&provider, &store, key, block)
            })
            .await
            .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_query", move |params, _ctx, _ext| {
        let provider = p.clone();
        let store = st.clone();
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
            tokio::task::spawn_blocking(move || run_query(&provider, &store, &q, &options))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_getEntityCount", move |params, _ctx, _ext| {
        let provider = p.clone();
        let store = st.clone();
        async move {
            // Optional `[{ query?, block? }]`; absent means "all, at the tip".
            let request: CountRequest = params
                .sequence()
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || entity_count(&provider, &store, request))
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

/// The block a read answers for: the tip when `block` is `None`, else that
/// block. Ahead of the tip is unanswerable.
fn resolve_block<Provider>(
    provider: &Provider,
    block: Option<u64>,
) -> Result<BlockNumber, ErrorObjectOwned>
where
    Provider: BlockNumReader,
{
    let tip = provider
        .best_block_number()
        .map_err(|e| internal_error(format!("best_block_number: {e:?}")))?;
    match block {
        None => Ok(tip),
        Some(n) if n <= tip => Ok(n),
        Some(n) => Err(block_unavailable(n, tip, "ahead of the chain tip")),
    }
}

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
            // U64, not U256: `entity_records` stores `$expiration` as a
            // U64 cell, and a predicate never crosses types — a U256 bound
            // would silently match nothing.
            value: AnnotVal::U64(block),
        }),
    )
}

/// Read one entity by key, at the tip or as of `block`. A past-BTL entity reads
/// as absent — see [`is_live`].
///
/// Answers in the same [`EntityData`] shape as `arkiv_query`, with every field
/// present ([`Projection::all`]): one entity model, one set of encodings, so an
/// SDK that reads through both methods decodes them the same way.
fn read_entity<Provider>(
    provider: &Provider,
    store: &HostStore,
    key: B256,
    block: Option<u64>,
) -> Result<Option<EntityData>, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let block_number = resolve_block(provider, block)?;
    let entity = store_reads::entity(&**store, store.head(), key.0)
        .map_err(|e| internal_error(format!("get entity: {e}")))?;
    Ok(entity
        .filter(|e| is_live(e.expires_at, block_number))
        .map(|e| entity_data_from(e, &Projection::all())))
}

/// Testing/debugging read that deliberately bypasses the logical expiry filter.
fn entity_exists_unfiltered<Provider>(
    provider: &Provider,
    store: &HostStore,
    key: B256,
    block: Option<u64>,
) -> Result<bool, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let _ = resolve_block(provider, block)?;
    Ok(store_reads::entity(&**store, store.head(), key.0)
        .map_err(|e| internal_error(format!("get entity: {e}")))?
        .is_some())
}

/// Parse `q`, evaluate it against the index (at the tip or as of
/// `options.atBlock`), and read the matched entities back.
///
/// One snapshot backs both stores: the index's `evaluate` resolves the query to
/// a page of keys, then the same snapshot is recovered (`into_backend`) and
/// reused to read those entities' full bytes — so the keys and the entities are
/// read from a single consistent state.
///
/// The query is bounded to entities still live at that block — see [`live_at`].
fn run_query<Provider>(
    provider: &Provider,
    store: &HostStore,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse, ErrorObjectOwned>
where
    Provider: BlockNumReader,
{
    let query = arkiv_query::parse(q).map_err(|e| query_error(&e))?;
    let projection = Projection::resolve(options.select.as_ref()).map_err(invalid_params)?;
    let page_size =
        arkiv_rpc_types::method::resolve_limit(options.limit).map_err(invalid_params)?;

    let at_block = match options.at_block {
        Some(BlockNumberOrTag::Number(n)) => Some(n),
        _ => None,
    };
    let block_number = resolve_block(provider, at_block)?;
    let binding = cursor::binding(q, block_number, &projection.fingerprint());
    let offset = match options.cursor.as_deref() {
        None => 0,
        Some(text) => cursor::decode(text, binding).map_err(|e| cursor_error(e.message()))?,
    };

    let limit = page_size;
    let entities = store_reads::query(
        &**store,
        store.head(),
        &live_at(query, block_number),
        offset,
        limit,
    )
    .map_err(|e| internal_error(format!("query: {e}")))?;

    // A full page may have more behind it; a short one is the end.
    let more = entities.len() as u64 == limit;
    let data: Vec<_> = entities
        .into_iter()
        .map(|e| entity_data_from(e, &projection))
        .collect();

    Ok(QueryResponse {
        data,
        block_number,
        cursor: more.then(|| cursor::encode(offset + limit, binding)),
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
    store: &HostStore,
    request: CountRequest,
) -> Result<u64, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let query = match &request.query {
        Some(text) => arkiv_query::parse(text).map_err(|e| query_error(&e))?,
        None => Query::All,
    };
    let block_number = resolve_block(provider, request.block)?;
    store_reads::count(&**store, store.head(), &live_at(query, block_number))
        .map_err(|e| internal_error(format!("count: {e}")))
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
