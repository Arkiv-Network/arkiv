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
mod request_limit;
pub mod store_reads;

pub use request_limit::{DEFAULT_MAX_CONCURRENT_REQUESTS, RequestLimit};

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use arkiv_interfaces::primitives::BlockNumber;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, Query};
use arkiv_interfaces::store::{CommitId, Sort, StoreError, StoreExt};
use arkiv_reth_statemanager::{BlockSeals, CommitWaitError, HostStore};
use arkiv_rpc_types::entity::{EntityData, Projection, entity_data_from};
use arkiv_rpc_types::method::{BlockTimingView, CountRequest, QueryOptions, QueryResponse};
use jsonrpsee::RpcModule;
use jsonrpsee::types::ErrorObjectOwned;
use reth_storage_api::{BlockNumReader, HeaderProvider};
use std::cell::Cell;
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::error::{block_unavailable, cursor_error, internal_error, invalid_params, query_error};
use crate::store_reads::ReadError;

/// Why a past block cannot be read: the store no longer holds its state.
const NOT_RETAINED: &str = "state for this block is not retained";

/// Why the tip cannot be read: its state will not reach the store.
const NOT_COMMITTED: &str = "state for this block is not committed";

/// How many `arkiv_*` reads run at once unless the node is told otherwise.
/// Each read holds one GolemDB read slot at a time. The node shares a fixed
/// number of slots, block import included, and with none left every read waits.
pub const DEFAULT_MAX_CONCURRENT_READS: usize = 32;

/// Build the `arkiv_*` [`RpcModule`], ready to merge into reth's rpc modules.
///
/// `provider` names the chain's head and answers block-header/number reads for
/// `arkiv_getBlockTiming`; entity state is read from `store`. `seals` signals
/// when a block's state is committed to `store`, which a read at the tip waits
/// for. At most `max_concurrent_reads` reads run at once; further calls wait
/// for a turn.
pub fn arkiv_module<Provider>(
    provider: Provider,
    store: HostStore,
    seals: Arc<BlockSeals>,
    max_concurrent_reads: usize,
) -> eyre::Result<RpcModule<Semaphore>>
where
    Provider: HeaderProvider + BlockNumReader + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(Semaphore::new(max_concurrent_reads));

    let p = provider.clone();
    let st = store.clone();
    let se = seals.clone();
    module.register_async_method("arkiv_getEntity", move |params, permits, _ext| {
        let provider = p.clone();
        let store = st.clone();
        let seals = se.clone();
        async move {
            // `[key]` at the tip, or `[key, block]` as of a past block.
            let mut seq = params.sequence();
            let key: B256 = seq
                .next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?;
            let block: Option<u64> = seq
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid block param: {e}")))?;
            run_read(permits, move || {
                read_entity(&provider, &store, &seals, key, block)
            })
            .await
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    let se = seals.clone();
    module.register_async_method("arkiv_debugEntityExists", move |params, permits, _ext| {
        let provider = p.clone();
        let store = st.clone();
        let seals = se.clone();
        async move {
            let mut seq = params.sequence();
            let key: B256 = seq
                .next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?;
            let block: Option<u64> = seq
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid block param: {e}")))?;
            run_read(permits, move || {
                entity_exists_unfiltered(&provider, &store, &seals, key, block)
            })
            .await
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    let se = seals.clone();
    module.register_async_method("arkiv_query", move |params, permits, _ext| {
        let provider = p.clone();
        let store = st.clone();
        let seals = se.clone();
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
            run_read(permits, move || {
                run_query(&provider, &store, &seals, &q, &options)
            })
            .await
        }
    })?;

    let p = provider.clone();
    let st = store.clone();
    module.register_async_method("arkiv_getEntityCount", move |params, permits, _ext| {
        let provider = p.clone();
        let store = st.clone();
        let seals = seals.clone();
        async move {
            // Optional `[{ query?, block? }]`; absent means "all, at the tip".
            let request: CountRequest = params
                .sequence()
                .optional_next()
                .map_err(|e| invalid_params(format!("invalid params: {e}")))?
                .unwrap_or_default();
            run_read(permits, move || {
                entity_count(&provider, &store, &seals, request)
            })
            .await
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
/// Each adopted commit is tagged with its block hash, so a block resolves to
/// its own commit — history without any per-store history machinery, and
/// bounded by the store's retention rather than by reth's pruning.
///
/// reth makes a block canonical a moment before adoption commits it, so a read
/// at the tip waits for the tip's commit. If that commit is not coming the read
/// is refused: the store's head holds an earlier block's state.
fn resolve_at<Provider>(
    provider: &Provider,
    store: &HostStore,
    seals: &BlockSeals,
    block: Option<u64>,
) -> Result<(BlockNumber, CommitId), ErrorObjectOwned>
where
    Provider: BlockNumReader,
{
    let head = provider
        .chain_info()
        .map_err(|e| internal_error(format!("chain_info: {e:?}")))?;
    let tip = head.best_number;
    let number = match block {
        None => tip,
        Some(n) if n <= tip => n,
        Some(n) => return Err(block_unavailable(n, tip, "ahead of the chain tip")),
    };
    if number == tip {
        // A head lookup that fails during the wait, kept to report as itself.
        let lookup_failure = Cell::new(None);
        let is_head = || match provider.chain_info() {
            Ok(info) => info.best_hash == head.best_hash,
            Err(error) => {
                lookup_failure.set(Some(error));
                false
            }
        };
        return match seals.wait_for_commit(store, head.best_hash.0, is_head) {
            Ok(commit) => Ok((number, commit)),
            Err(CommitWaitError::NotComing(_)) => Err(match lookup_failure.take() {
                Some(error) => internal_error(format!("chain_info: {error:?}")),
                None => block_unavailable(number, tip, NOT_COMMITTED),
            }),
            Err(error) => Err(internal_error(format!(
                "resolve commit for block {number}: {error}"
            ))),
        };
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
    seals: &BlockSeals,
    key: B256,
    block: Option<u64>,
) -> Result<Option<EntityData>, ErrorObjectOwned>
where
    Provider: BlockNumReader,
{
    read_at(
        || resolve_at(provider, store, seals, block),
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
    seals: &BlockSeals,
    key: B256,
    block: Option<u64>,
) -> Result<bool, ErrorObjectOwned>
where
    Provider: BlockNumReader,
{
    read_at(
        || resolve_at(provider, store, seals, block),
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
    seals: &BlockSeals,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse, ErrorObjectOwned>
where
    Provider: BlockNumReader,
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
        || resolve_at(provider, store, seals, at_block),
        |block_number, at| {
            let binding = cursor::binding(q, block_number, &projection.fingerprint());
            let offset = match options.cursor.as_deref() {
                None => options.offset.unwrap_or(0),
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
    seals: &BlockSeals,
    request: CountRequest,
) -> Result<u64, ErrorObjectOwned>
where
    Provider: BlockNumReader,
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
        || resolve_at(provider, store, seals, request.block),
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

    use arkiv_interfaces::entity::Entity;
    use arkiv_interfaces::entity_records;
    use arkiv_interfaces::store::reference::MemStore;
    use arkiv_interfaces::store::{RecordKey, Store};
    use arkiv_reth_statemanager::{ExecutionKey, OwnedBranch};
    use reth_storage_api::BlockHashReader;
    use reth_storage_api::errors::provider::{ProviderError, ProviderResult};
    use std::fmt::Debug;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;

    /// The entity block 2 creates.
    const ENTITY: [u8; 32] = [0x11; 32];

    /// Block `n`'s hash.
    fn hash(n: u64) -> B256 {
        B256::repeat_byte(n as u8 + 1)
    }

    /// reth, as far as a read can tell: block `n` is `hash(n)`, and block 2 is
    /// the head.
    #[derive(Default)]
    struct Chain {
        /// How often the head was asked for.
        asked: AtomicU64,
        /// Run when the head is asked for a second time. A read asks again
        /// only after finding its block uncommitted, just before it waits.
        about_to_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// Whether asking for the head fails.
        broken: AtomicBool,
    }

    impl BlockHashReader for Chain {
        fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
            Ok((number <= 2).then(|| hash(number)))
        }

        fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
            Ok((start..end).map(hash).collect())
        }
    }

    impl BlockNumReader for Chain {
        fn chain_info(&self) -> ProviderResult<reth_chainspec::ChainInfo> {
            // The answer is settled before `about_to_wait` runs, so what that
            // sets off reaches a read only once it is waiting.
            let head = if self.broken.load(Ordering::SeqCst) {
                Err(ProviderError::BestBlockNotFound)
            } else {
                Ok(reth_chainspec::ChainInfo {
                    best_hash: hash(2),
                    best_number: 2,
                })
            };
            if self.asked.fetch_add(1, Ordering::SeqCst) > 0
                && let Some(about_to_wait) = self.about_to_wait.lock().unwrap().take()
            {
                about_to_wait();
            }
            head
        }

        fn best_block_number(&self) -> ProviderResult<u64> {
            Ok(2)
        }

        fn last_block_number(&self) -> ProviderResult<u64> {
            Ok(2)
        }

        fn block_number(&self, block: B256) -> ProviderResult<Option<u64>> {
            Ok((0..=2).find(|&n| hash(n) == block))
        }
    }

    /// A node between reth making block 2 canonical and adoption committing
    /// it. The store holds blocks 0 and 1. Block 2 created [`ENTITY`] and is
    /// sealed, and reth already reports it as the head and serves its receipt.
    struct BeforeAdoption {
        chain: Chain,
        store: HostStore,
        seals: BlockSeals,
        /// The state root block 2 sealed.
        state_root: [u8; 32],
    }

    impl BeforeAdoption {
        fn new() -> Self {
            let store: HostStore = Arc::new(MemStore::new());
            for n in 0..=1 {
                let branch = store.begin(None).expect("begin");
                store.commit_tagged(branch, hash(n).0).expect("commit");
            }
            let entity = Entity {
                key: ENTITY,
                created_at: 2,
                updated_at: 2,
                expires_at: 1_000,
                ..Entity::default()
            };
            let branch = OwnedBranch::begin(&store).expect("begin");
            store
                .create(
                    branch.id(),
                    RecordKey::from_entity(ENTITY),
                    entity_records::to_cells(&entity).expect("cells"),
                    None,
                )
                .expect("create");
            let seals = BlockSeals::default();
            let state_root = seals
                .seal(ExecutionKey::new(2, vec![]), branch)
                .expect("seal")
                .state_root;
            Self {
                chain: Chain::default(),
                store,
                seals,
                state_root,
            }
        }

        /// Adoption commits block 2.
        fn adopt(&self) {
            self.seals
                .adopt(&self.store, 2, hash(2).0, self.state_root, &[])
                .expect("adopt")
                .expect("block 2 was sealed");
            self.seals.finish_adoption(hash(2).0);
        }

        /// Adoption finishes with block 2 without committing it.
        fn finish_without_commit(&self) {
            self.seals.finish_adoption(hash(2).0);
        }

        /// What `read` answers when `adoption` runs only once the read has
        /// found block 2 uncommitted and is about to wait for it. Nothing here
        /// depends on timing: a read that answers without waiting fails.
        fn read_during<T: Debug + Send + 'static>(
            &self,
            read: impl FnOnce(&Self) -> T + Send,
            adoption: impl FnOnce(&Self),
        ) -> T {
            // `None` when the read is about to wait, then `Some` of its answer.
            let (events, seen) = mpsc::channel();
            let waiting = events.clone();
            *self.chain.about_to_wait.lock().unwrap() = Some(Box::new(move || {
                let _ = waiting.send(None);
            }));
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    let _ = events.send(Some(read(self)));
                });
                if let Some(answer) = seen.recv().expect("the read reports") {
                    panic!("the read answered before block 2 was adopted: {answer:?}");
                }
                adoption(self);
                seen.recv()
                    .expect("the read reports")
                    .expect("the read waits once")
            })
        }

        /// `arkiv_query` for everything at the tip: the block it answers for
        /// and how many entities it returns.
        fn query_all(&self) -> Result<(u64, usize), i32> {
            run_query(
                &self.chain,
                &self.store,
                &self.seals,
                "*",
                &QueryOptions::default(),
            )
            .map(|answer| (answer.block_number, answer.data.len()))
            .map_err(|error| error.code())
        }

        /// `arkiv_getEntity` for [`ENTITY`], at the tip or as of `block`.
        fn entity(&self, block: Option<u64>) -> Result<bool, i32> {
            read_entity(
                &self.chain,
                &self.store,
                &self.seals,
                B256::from(ENTITY),
                block,
            )
            .map(|entity| entity.is_some())
            .map_err(|error| error.code())
        }

        /// `arkiv_getEntityCount` of everything at the tip.
        fn count_all(&self) -> Result<u64, i32> {
            let everything = CountRequest::default();
            entity_count(&self.chain, &self.store, &self.seals, everything)
                .map_err(|error| error.code())
        }
    }

    /// A query at the tip waits for adoption to commit block 2, so it answers
    /// for block 2 with block 2's entity. Without the wait it would answer for
    /// block 2 with the state of block 1.
    #[test]
    fn a_query_at_the_tip_waits_for_the_tips_commit() {
        let node = BeforeAdoption::new();
        let answer = node.read_during(BeforeAdoption::query_all, BeforeAdoption::adopt);
        assert_eq!(answer, Ok((2, 1)));
    }

    /// The same for one entity, read at the tip and read as of the tip's
    /// number: a client that reads on seeing a receipt finds what it created.
    #[test]
    fn an_entity_read_at_the_tip_waits_for_the_tips_commit() {
        for block in [None, Some(2)] {
            let node = BeforeAdoption::new();
            let found = node.read_during(|node| node.entity(block), BeforeAdoption::adopt);
            assert_eq!(found, Ok(true), "as of {block:?}");
        }
    }

    /// And for a count, whose answer is a bare number.
    #[test]
    fn a_count_at_the_tip_waits_for_the_tips_commit() {
        let node = BeforeAdoption::new();
        let count = node.read_during(BeforeAdoption::count_all, BeforeAdoption::adopt);
        assert_eq!(count, Ok(1));
    }

    /// Adoption can finish with a block without committing it. No read at the
    /// tip then answers from the store's head, which holds block 1. An entity
    /// or a count carries no block number, so block 1's answer would pass for
    /// the state the receipt describes.
    #[test]
    fn a_read_at_a_tip_that_is_not_committed_is_refused() {
        type Read = fn(&BeforeAdoption) -> Option<i32>;
        let reads: [(&str, Read); 4] = [
            ("an entity at latest", |node| node.entity(None).err()),
            ("an entity as of block 2", |node| node.entity(Some(2)).err()),
            ("a query at latest", |node| node.query_all().err()),
            ("a count at latest", |node| node.count_all().err()),
        ];
        for (name, read) in reads {
            let node = BeforeAdoption::new();
            let error = node.read_during(read, BeforeAdoption::finish_without_commit);
            assert_eq!(error, Some(rpc_error_codes::BLOCK_UNAVAILABLE), "{name}");
        }
    }

    /// A head lookup that fails while a read waits is reported as that
    /// failure, not as a block the store will not commit.
    #[test]
    fn a_head_lookup_that_fails_during_the_wait_is_an_internal_error() {
        let node = BeforeAdoption::new();
        let failed = node.read_during(BeforeAdoption::query_all, |node| {
            node.chain.broken.store(true, Ordering::SeqCst);
            // Adoption finishing with any block wakes the read, which asks for
            // the head again.
            node.seals.finish_adoption(hash(1).0);
        });
        assert_eq!(failed, Err(INTERNAL_ERROR_CODE));
    }
}
