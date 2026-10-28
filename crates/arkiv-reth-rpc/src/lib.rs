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
use arkiv_interfaces::store::{CommitId, Sort, Store, StoreError, StoreExt};
use arkiv_reth_statemanager::HostStore;
use arkiv_rpc_types::entity::{EntityData, Projection, entity_data_from};
use arkiv_rpc_types::method::{BlockTimingView, CountRequest, QueryOptions, QueryResponse};
use jsonrpsee::RpcModule;
use jsonrpsee::types::ErrorObjectOwned;
use reth_storage_api::{BlockNumReader, HeaderProvider, StateProviderFactory};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::error::{block_unavailable, cursor_error, internal_error, invalid_params, query_error};
use crate::store_reads::ReadError;

/// Why a past block cannot be read: the store no longer holds its state.
const NOT_RETAINED: &str = "state for this block is not retained";

/// How many `arkiv_*` reads run at once unless the node is told otherwise.
/// Each read holds one GolemDB read slot at a time. The node shares a fixed
/// number of slots, block import included, and with none left every read fails.
pub const DEFAULT_MAX_CONCURRENT_READS: usize = 32;

/// Build the `arkiv_*` [`RpcModule`], ready to merge into reth's rpc modules.
///
/// `provider` hands out a fresh state snapshot per call (`StateProviderFactory`) and
/// answers block-header/number reads for `arkiv_getBlockTiming`. At most
/// `max_concurrent_reads` reads run at once; further calls wait for a turn.
pub fn arkiv_module<Provider>(
    provider: Provider,
    store: HostStore,
    max_concurrent_reads: usize,
) -> eyre::Result<RpcModule<Semaphore>>
where
    Provider:
        StateProviderFactory + HeaderProvider + BlockNumReader + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(Semaphore::new(max_concurrent_reads));

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_getEntity", move |params, permits, _ext| {
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
            run_read(permits, move || read_entity(&provider, &store, key, block)).await
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_debugEntityExists", move |params, permits, _ext| {
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
            run_read(permits, move || {
                entity_exists_unfiltered(&provider, &store, key, block)
            })
            .await
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_query", move |params, permits, _ext| {
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
            run_read(permits, move || run_query(&provider, &store, &q, &options)).await
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_getEntityCount", move |params, permits, _ext| {
        let provider = p.clone();
        let store = st.clone();
        async move {
            // Optional `[{ query?, block? }]`; absent means "all, at the tip".
            let request: CountRequest = params
                .sequence()
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?
                .unwrap_or_default();
            run_read(permits, move || entity_count(&provider, &store, request)).await
        }
    })?;

    let p = provider;
    module.register_async_method("arkiv_getBlockTiming", move |_params, permits, _ext| {
        let provider = p.clone();
        async move { run_read(permits, move || block_timing(&provider)).await }
    })?;

    Ok(module)
}

/// Run `read` off the async runtime once one of `permits` is free. Reads hit
/// MDBX, so they block. The permit lives as long as the read, even when the
/// caller goes away first.
async fn run_read<T, F>(permits: Arc<Semaphore>, read: F) -> Result<T, ErrorObjectOwned>
where
    F: FnOnce() -> Result<T, ErrorObjectOwned> + Send + 'static,
    T: Send + 'static,
{
    let permit = permits
        .acquire_owned()
        .await
        .map_err(|e| internal_error(format!("read permit: {e}")))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        read()
    })
    .await
    .map_err(|e| internal_error(format!("task join: {e}")))?
}

/// The block a read answers for and the store commit that holds its state:
/// the tip when `block` is `None`, else that block.
///
/// Each adopted commit is tagged with its block hash, so a past block resolves
/// to a past commit — history without any per-store history machinery, and
/// bounded by the store's retention rather than by reth's pruning.
fn resolve_at<Provider>(
    provider: &Provider,
    store: &HostStore,
    block: Option<u64>,
) -> Result<(BlockNumber, CommitId), ErrorObjectOwned>
where
    Provider: BlockNumReader + HeaderProvider,
{
    let tip = provider
        .best_block_number()
        .map_err(|e| internal_error(format!("best_block_number: {e:?}")))?;
    let number = match block {
        None => tip,
        Some(n) if n <= tip => n,
        Some(n) => return Err(block_unavailable(n, tip, "ahead of the chain tip")),
    };
    if number == tip {
        return Ok((number, store.head()));
    }
    let hash = provider
        .block_hash(number)
        .map_err(|e| internal_error(format!("block_hash({number}): {e:?}")))?
        .ok_or_else(|| block_unavailable(number, tip, "no header for this block"))?;
    // A store may still find a block's commit after dropping its state:
    // GolemDB keeps only the head's.
    let commit = store
        .commit_by_tag(hash.0)
        .map_err(|e| internal_error(format!("resolve commit for block {number}: {e:?}")))?
        .filter(|&commit| commit >= store.retention().0)
        .ok_or_else(|| block_unavailable(number, tip, NOT_RETAINED))?;
    Ok((number, commit))
}

/// Why a read at a commit failed.
enum ReadFailure {
    /// The store dropped the commit's state before the read ran.
    StateDropped,
    /// Any other failure, as the caller will see it.
    Other(ErrorObjectOwned),
}

impl From<ErrorObjectOwned> for ReadFailure {
    fn from(error: ErrorObjectOwned) -> Self {
        Self::Other(error)
    }
}

/// A store read's failure, with `context` naming the read.
fn store_failure(context: &'static str) -> impl FnOnce(ReadError) -> ReadFailure {
    move |error| match error {
        ReadError::Store(StoreError::HandleInvalid) => ReadFailure::StateDropped,
        error => ReadFailure::Other(internal_error(format!("{context}: {error}"))),
    }
}

/// Run `read` at the block and commit `resolve` finds.
///
/// A store that keeps only the head's state, as GolemDB does, drops a commit
/// when the next one lands, which can happen between finding the commit and
/// reading it. The read then runs once more on a fresh `resolve`: a read at the
/// tip moves to the new tip, and a past block is refused as not retained.
fn read_at<T>(
    resolve: impl Fn() -> Result<(BlockNumber, CommitId), ErrorObjectOwned>,
    read: impl Fn(BlockNumber, CommitId) -> Result<T, ReadFailure>,
) -> Result<T, ErrorObjectOwned> {
    let (number, commit) = resolve()?;
    match read(number, commit) {
        Ok(value) => return Ok(value),
        Err(ReadFailure::Other(error)) => return Err(error),
        Err(ReadFailure::StateDropped) => {}
    }
    let (number, commit) = resolve()?;
    read(number, commit).map_err(|error| match error {
        ReadFailure::StateDropped => block_unavailable(number, number, NOT_RETAINED),
        ReadFailure::Other(error) => error,
    })
}

/// The rule: an entity is live at `block` while its expiry block is still ahead.
fn is_live(expires_at: BlockNumber, block: BlockNumber) -> bool {
    expires_at > block
}

/// The same rule as a query bound — `query AND $expiresAt > block`.
///
/// `$expiresAt` is an indexed built-in, so this filters inside the index rather
/// than after the fact: pages come back full instead of pocked with holes, and
/// the match-set cardinality behind `arkiv_getEntityCount` excludes dead rows.
fn live_at(query: Query, block: BlockNumber) -> Query {
    Query::And(vec![
        query,
        Query::Gt {
            key: AnnotKey::BuiltIn(BuiltIn::ExpiresAt),
            // U64, not U256: `entity_records` stores `$expiresAt` as a
            // U64 cell, and a predicate never crosses types — a U256 bound
            // would silently match nothing.
            value: AnnotVal::U64(block),
        },
    ])
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
    Provider: StateProviderFactory + BlockNumReader + HeaderProvider,
{
    read_at(
        || resolve_at(provider, store, block),
        |block_number, at| {
            let entity =
                store_reads::entity(&**store, at, key.0).map_err(store_failure("get entity"))?;
            Ok(entity
                .filter(|e| is_live(e.expires_at, block_number))
                .map(|e| entity_data_from(e, &Projection::all())))
        },
    )
}

/// Testing/debugging read that deliberately bypasses the logical expiry filter.
fn entity_exists_unfiltered<Provider>(
    provider: &Provider,
    store: &HostStore,
    key: B256,
    block: Option<u64>,
) -> Result<bool, ErrorObjectOwned>
where
    Provider: StateProviderFactory + BlockNumReader + HeaderProvider,
{
    read_at(
        || resolve_at(provider, store, block),
        |_, at| {
            Ok(store_reads::entity(&**store, at, key.0)
                .map_err(store_failure("get entity"))?
                .is_some())
        },
    )
}

/// Parse `q`, evaluate it against the index (at the tip or as of
/// `options.atBlock`), and read the matched entities back.
///
/// One snapshot backs both stores: the index's `evaluate` resolves the query to
/// a page of keys, then the same snapshot is recovered (`into_backend`) and
/// reused to read those entities' full bytes — so the keys and the entities are
/// read from a single consistent state.
///
/// The query is bounded to entities still live at that block — see [`live_at`]
/// — and its matches come in the order its `ORDER BY` asks for.
fn run_query<Provider>(
    provider: &Provider,
    store: &HostStore,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse, ErrorObjectOwned>
where
    Provider: BlockNumReader + HeaderProvider,
{
    let statement = arkiv_query::parse_statement(q).map_err(|e| query_error(&e))?;
    let sort = statement.order.as_ref().map(Sort::from);
    let projection = Projection::resolve(options.select.as_ref()).map_err(invalid_params)?;
    let page_size =
        arkiv_rpc_types::method::resolve_limit(options.limit).map_err(invalid_params)?;

    // Only a hex number or `latest` names a block whose state is settled.
    // Anything else — `pending`, `safe`, `finalized` — would quietly answer
    // from the tip, which is a wrong answer rather than a refused one.
    let at_block = match options.at_block {
        None | Some(BlockNumberOrTag::Latest) => None,
        Some(BlockNumberOrTag::Number(n)) => Some(n),
        Some(other) => {
            return Err(invalid_params(format!(
                "atBlock tag {other:?} not supported; use a hex block number or 'latest'"
            )));
        }
    };
    read_at(
        || resolve_at(provider, store, at_block),
        |block_number, at| {
            let binding = cursor::binding(q, block_number, &projection.fingerprint());
            let offset = match options.cursor.as_deref() {
                None => 0,
                Some(text) => {
                    cursor::decode(text, binding).map_err(|e| cursor_error(e.message()))?
                }
            };

            let live = live_at(statement.filter.clone(), block_number);
            // `count` alone selects no entity, so there is no page to read:
            // the count is the whole answer, with no cursor to a next page.
            if projection.is_count_only() {
                let count =
                    store_reads::count(&**store, at, &live).map_err(store_failure("count"))?;
                return Ok(QueryResponse {
                    data: Vec::new(),
                    block_number,
                    cursor: None,
                    count: Some(count),
                });
            }

            let limit = page_size;
            let page = store_reads::query(
                &**store,
                at,
                &live,
                sort.as_ref(),
                offset,
                limit,
                projection.count,
            )
            .map_err(store_failure("query"))?;

            // A full page may have more behind it; a short one is the end.
            let more = page.entities.len() as u64 == limit;
            let data: Vec<_> = page
                .entities
                .into_iter()
                .map(|e| entity_data_from(e, &projection))
                .collect();

            Ok(QueryResponse {
                data,
                block_number,
                cursor: more.then(|| cursor::encode(offset + limit, binding)),
                count: page.total_matched,
            })
        },
    )
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
    Provider: StateProviderFactory + BlockNumReader + HeaderProvider,
{
    let query = match &request.query {
        // A count has no order, so an `ORDER BY` is read and set aside: the
        // query string a page was asked with counts as it is.
        Some(text) => {
            arkiv_query::parse_statement(text)
                .map_err(|e| query_error(&e))?
                .filter
        }
        None => Query::All,
    };
    read_at(
        || resolve_at(provider, store, request.block),
        |block_number, at| {
            store_reads::count(&**store, at, &live_at(query.clone(), block_number))
                .map_err(store_failure("count"))
        },
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::rpc_error_codes;
    use jsonrpsee::types::error::INTERNAL_ERROR_CODE;
    use std::cell::Cell;

    /// Finds block `first` and its commit, then the next block on each call, as
    /// a chain that moves on between calls would.
    fn tip_from(first: u64) -> impl Fn() -> Result<(BlockNumber, CommitId), ErrorObjectOwned> {
        let next = Cell::new(first);
        move || {
            let number = next.replace(next.get() + 1);
            Ok((number, CommitId(number)))
        }
    }

    /// A read that lost its commit to the next block runs again at that block.
    #[test]
    fn a_lost_commit_is_read_again_at_the_new_tip() {
        let read = read_at(tip_from(5), |number, commit| match commit {
            CommitId(5) => Err(ReadFailure::StateDropped),
            _ => Ok((number, commit)),
        });
        assert_eq!(read.unwrap(), (6, CommitId(6)));
    }

    /// A commit lost on both tries is refused as not retained, not as internal.
    #[test]
    fn a_commit_lost_twice_is_not_retained() {
        let read = read_at(tip_from(5), |_, _| Err::<(), _>(ReadFailure::StateDropped));
        assert_eq!(read.unwrap_err().code(), rpc_error_codes::BLOCK_UNAVAILABLE);
    }

    /// Any other failure reaches the caller as it is, without a second try.
    #[test]
    fn other_failures_are_not_retried() {
        let tries = Cell::new(0);
        let read = read_at(tip_from(5), |_, _| {
            tries.set(tries.get() + 1);
            Err::<(), _>(ReadFailure::Other(internal_error("broken".into())))
        });
        assert_eq!(read.unwrap_err().code(), INTERNAL_ERROR_CODE);
        assert_eq!(tries.get(), 1);
    }

    /// Only a stale handle means the store dropped the commit's state.
    #[test]
    fn only_a_stale_handle_means_the_state_was_dropped() {
        let dropped = store_failure("get entity")(ReadError::Store(StoreError::HandleInvalid));
        assert!(matches!(dropped, ReadFailure::StateDropped));
        let other = store_failure("get entity")(ReadError::Store(StoreError::NotFound));
        assert!(matches!(other, ReadFailure::Other(_)));
    }

    /// No more reads run at once than there are permits; the rest wait their turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_beyond_the_permits_wait() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let permits = Arc::new(Semaphore::new(2));
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let reads: Vec<_> = (0..8)
            .map(|_| {
                let (running, most) = (running.clone(), most.clone());
                tokio::spawn(run_read(permits.clone(), move || {
                    most.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }))
            })
            .collect();
        for read in reads {
            read.await.unwrap().unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 2);
    }
}
