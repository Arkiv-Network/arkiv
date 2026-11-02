//! The Arkiv RPC surface — the `arkiv_*` methods a node serves to clients.
//!
//! This is the external contract the SDK depends on, expressed in domain types.
//! It differs from [`QueryProcessor`](crate::query::QueryProcessor): queries
//! arrive as **text** (the query language), so a host parses each one — surfacing
//! parse errors through `Error` — before evaluating it. Transport and
//! serialization (JSON-RPC, HTTP) are the host's concern and live in its crate.

use crate::entity::Entity;
use crate::primitives::{BlockNumber, EntityKey};
use crate::query::{PageParams, QueryResult};

/// The `arkiv_*` methods every node serves, at the current tip.
pub trait ArkivRpc {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// `arkiv_query`: parse and evaluate `query`, returning one page of results.
    fn query(&self, query: &str, page: PageParams) -> Result<QueryResult, Self::Error>;

    /// `arkiv_getEntity`: fetch one entity by key, or `None` if there is none.
    fn get_entity(&self, key: EntityKey) -> Result<Option<Entity>, Self::Error>;
}

/// Optional: the same methods against a **past block**, for nodes that serve
/// history (see [`HistoricalQuery`](crate::query::HistoricalQuery)).
pub trait ArkivHistoricalRpc: ArkivRpc {
    /// `arkiv_query` as of block `at`.
    fn query_at(
        &self,
        query: &str,
        page: PageParams,
        at: BlockNumber,
    ) -> Result<QueryResult, Self::Error>;

    /// `arkiv_getEntity` as of block `at`.
    fn get_entity_at(&self, key: EntityKey, at: BlockNumber)
    -> Result<Option<Entity>, Self::Error>;
}
