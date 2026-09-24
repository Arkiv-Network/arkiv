//! The `arkiv_*` JSON-RPC namespace, served over reth state.
//!
//! The read-path counterpart to `arkiv-reth-executor`: [`arkiv_module`] builds a
//! jsonrpsee [`RpcModule`] the binary merges into reth's rpc modules, answering
//! `arkiv_getEntity`, `arkiv_query`, `arkiv_getEntityCount` and
//! `arkiv_getBlockTiming`.
//!
//! Each read opens a [`SnapshotView`] of the requested state — the tip, or a
//! past block: the reth snapshot's anchor slot names the database root, and
//! the Arkiv node store still holds every root a block committed, so a
//! historical snapshot carries a historical index for free. Within one
//! `arkiv_query` the *same* view resolves the query and reads the matched
//! entities, so a page is always internally consistent.
//!
//! Wire shapes are the client's too and live in [`arkiv_rpc_types`]; this crate
//! holds only what a server decides.

pub mod cursor;
pub mod error;
pub mod snapshot;

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, hex};
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, PageParams, Query};
use arkiv_rpc_types::entity::{EntityData, Projection, entity_data_from};
use arkiv_rpc_types::method::{BlockTimingView, CountRequest, QueryOptions, QueryResponse};
use arkiv_store::{ArkivDb, DbView, evaluate_page};
use jsonrpsee::RpcModule;
use jsonrpsee::types::ErrorObjectOwned;
use reth_storage_api::{BlockNumReader, HeaderProvider, StateProviderBox, StateProviderFactory};
use std::sync::Arc;

use crate::error::{block_unavailable, cursor_error, internal_error, invalid_params, query_error};
use crate::snapshot::SnapshotView;

/// Build the `arkiv_*` [`RpcModule`], ready to merge into reth's rpc modules.
///
/// `provider` hands out a fresh state snapshot per call (`StateProviderFactory`) and
/// answers block-header/number reads for `arkiv_getBlockTiming`.
pub fn arkiv_module<Provider>(provider: Provider, db: Arc<ArkivDb>) -> eyre::Result<RpcModule<()>>
where
    Provider:
        StateProviderFactory + HeaderProvider + BlockNumReader + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(());

    let p = provider.clone();
    let d = db.clone();
    module.register_async_method("arkiv_getEntity", move |params, _ctx, _ext| {
        let provider = p.clone();
        let db = d.clone();
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
            tokio::task::spawn_blocking(move || read_entity(&provider, db, key, block))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    let d = db.clone();
    module.register_async_method("arkiv_debugEntityExists", move |params, _ctx, _ext| {
        let provider = p.clone();
        let db = d.clone();
        async move {
            let mut seq = params.sequence();
            let key: B256 = seq
                .next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?;
            let block: Option<u64> = seq
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid block param: {e}")))?;
            tokio::task::spawn_blocking(move || entity_exists_unfiltered(&provider, db, key, block))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    let d = db.clone();
    module.register_async_method("arkiv_query", move |params, _ctx, _ext| {
        let provider = p.clone();
        let db = d.clone();
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
            tokio::task::spawn_blocking(move || run_query(&provider, db, &q, &options))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
        }
    })?;

    let p = provider.clone();
    let d = db;
    module.register_async_method("arkiv_getEntityCount", move |params, _ctx, _ext| {
        let provider = p.clone();
        let db = d.clone();
        async move {
            // Optional `[{ query?, block? }]`; absent means "all, at the tip".
            let request: CountRequest = params
                .sequence()
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || entity_count(&provider, db, request))
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
///
/// Answers in the same [`EntityData`] shape as `arkiv_query`, with every field
/// present ([`Projection::all`]): one entity model, one set of encodings, so an
/// SDK that reads through both methods decodes them the same way.
fn read_entity<Provider>(
    provider: &Provider,
    db: Arc<ArkivDb>,
    key: B256,
    block: Option<u64>,
) -> Result<Option<EntityData>, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let (state, block_number) = resolve_state(provider, block)?;
    let snapshot = open_snapshot(&state, db)?;
    let entity = snapshot
        .view()
        .entity(&key.0)
        .map_err(|e| internal_error(format!("get entity: {e:?}")))?;
    Ok(entity
        .filter(|e| is_live(e.expires_at, block_number))
        .map(|e| entity_data_from(e, &Projection::all())))
}

/// Testing/debugging read that deliberately bypasses the logical expiry filter.
fn entity_exists_unfiltered<Provider>(
    provider: &Provider,
    db: Arc<ArkivDb>,
    key: B256,
    block: Option<u64>,
) -> Result<bool, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let (state, _) = resolve_state(provider, block)?;
    let snapshot = open_snapshot(&state, db)?;
    Ok(snapshot
        .view()
        .entity(&key.0)
        .map_err(|e| internal_error(format!("get entity: {e:?}")))?
        .is_some())
}

/// The Arkiv database as of `state`.
fn open_snapshot(
    state: &StateProviderBox,
    db: Arc<ArkivDb>,
) -> Result<SnapshotView, ErrorObjectOwned> {
    SnapshotView::open(state, db).map_err(|e| internal_error(format!("open snapshot: {e}")))
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
    let tip = provider
        .best_block_number()
        .map_err(|e| internal_error(format!("best_block_number: {e:?}")))?;

    match at_block {
        None | Some(BlockNumberOrTag::Latest) => {
            let state = provider
                .latest()
                .map_err(|e| internal_error(format!("latest state: {e:?}")))?;
            Ok((state, tip))
        }
        Some(BlockNumberOrTag::Number(n)) => {
            // Ahead of the tip is unanswerable; so is a block whose history the
            // node has pruned. Both are "outside the retained range" to a caller.
            if n > tip {
                return Err(block_unavailable(n, tip, "ahead of the chain tip"));
            }
            let state = provider
                .history_by_block_number(n)
                .map_err(|_| block_unavailable(n, tip, "state for this block is not retained"))?;
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
/// One view backs both steps: the query resolves to a page of keys, then the
/// same view reads those entities — so the keys and the entities come from a
/// single consistent state.
///
/// The query is bounded to entities still live at that block — see [`live_at`].
fn run_query<Provider>(
    provider: &Provider,
    db: Arc<ArkivDb>,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let query = arkiv_query::parse(q).map_err(|e| query_error(&e))?;
    let projection = Projection::resolve(options.select.as_ref()).map_err(invalid_params)?;
    let page_size =
        arkiv_rpc_types::method::resolve_limit(options.limit).map_err(invalid_params)?;

    // Resolve the block before the cursor: a cursor is bound to the block it was
    // issued against, so "latest" has to become a number first.
    let (state, block_number) = snapshot_for(provider, options.at_block)?;

    let binding = cursor::binding(q, block_number, &projection.fingerprint());
    let resume_from = match options.cursor.as_deref() {
        None => None,
        Some(text) => Some(cursor::decode(text, binding).map_err(|e| cursor_error(e.message()))?),
    };

    let snapshot = open_snapshot(&state, db)?;
    // Count every node lookup: the index walks and the page's entity reads.
    let counter = snapshot.counting();
    let view = DbView::at(&counter, *snapshot.roots());
    let matches = evaluate_page(
        &view,
        &live_at(query, block_number),
        PageParams {
            page_size,
            cursor: resume_from,
        },
    )
    .map_err(|e| internal_error(format!("evaluate: {e:?}")))?;

    let mut data = Vec::with_capacity(matches.keys.len());
    for key in &matches.keys {
        // A key in the index but missing from the entity trie is a store
        // inconsistency, not a normal "no such entity" — surface it.
        let entity = view
            .entity(key)
            .map_err(|e| internal_error(format!("get entity: {e:?}")))?
            .ok_or_else(|| {
                internal_error(format!(
                    "indexed entity {} missing from store",
                    hex_prefixed(key)
                ))
            })?;
        data.push(entity_data_from(entity, &projection));
    }

    Ok(QueryResponse {
        data,
        block_number,
        cursor: matches
            .next_cursor
            .map(|offset| cursor::encode(offset, binding)),
        nodes_read: counter.reads(),
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
    db: Arc<ArkivDb>,
    request: CountRequest,
) -> Result<u64, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader,
{
    let query = match &request.query {
        Some(text) => arkiv_query::parse(text).map_err(|e| query_error(&e))?,
        None => Query::All,
    };
    let (state, block_number) = resolve_state(provider, request.block)?;
    let snapshot = open_snapshot(&state, db)?;
    let matches = evaluate_page(
        &snapshot.view(),
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

fn hex_prefixed(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}
