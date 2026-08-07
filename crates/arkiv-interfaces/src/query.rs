//! Queries: the query language, and the trait that answers it.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::entity::{AttributeValue, Entity};
use crate::primitives::{BlockNumber, EntityKey, Gas};
use crate::state::{AuxiliaryStore, EntityStore, HistoricalAuxiliaryStore, HistoricalEntityStore};

/// A query — a tree of predicates over an entity's attributes.
///
/// The leaves compare one attribute (`key`) against a typed [`AnnotVal`]. The
/// branches (`And`/`Or`/`Not`) combine them.
///
/// Every leaf carries a *typed* value, and a predicate matches only an attribute
/// of that same type: `level >= i32(10)` never matches a `u256`-typed `level`.
/// The host gets this for free — the type is mixed into the index key — but it is
/// the language's rule, not an implementation detail.
///
/// There is deliberately **no `!=` variant**. Value-negation restricted to
/// "attribute is set with this type, and differs" needs a per-`(attribute, type)`
/// presence index the host does not maintain, so the language omits the operator
/// rather than silently answering the wider [`Not`](Self::Not) complement. See
/// `docs/v1-typed-query-scope.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Matches every live entity (`*`).
    All,
    /// Matches entities whose `key` equals `value`.
    Eq { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is greater than `value`.
    Gt { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is greater than or equal to `value`.
    Gte { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is less than `value`.
    Lt { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is less than or equal to `value`.
    Lte { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` starts with `value` — the `STARTSWITH`
    /// operator, a raw UTF-8 byte prefix with no normalization.
    StartsWith { key: AnnotKey, value: AnnotVal },
    /// Matches entities satisfying both subqueries.
    And(Box<Query>, Box<Query>),
    /// Matches entities satisfying either subquery.
    Or(Box<Query>, Box<Query>),
    /// Matches entities not satisfying the subquery — the full complement,
    /// evaluated against the live-entity set.
    Not(Box<Query>),
}

/// What a predicate matches on: a built-in field, or a user attribute by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotKey {
    BuiltIn(BuiltIn),
    User(String),
}

/// The built-in (`$`-prefixed) fields you can query by name.
///
/// These are the *language's* names. The byte strings they are indexed under are
/// the [`annotations`](crate::entity::annotations) constants, which a host maps
/// them to — the two are deliberately decoupled, because the annotation bytes are
/// mixed into index-bucket addresses and renaming one relocates on-chain state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltIn {
    /// `$owner` — the current owner's address.
    Owner,
    /// `$creator` — the creating address, immutable.
    Creator,
    /// `$key` — the entity key.
    Key,
    /// `$expiresAt` — the expiry block.
    ExpiresAt,
    /// `$createdAt` — the creation block.
    CreatedAt,
    /// `$contentType` — the payload's MIME string.
    ContentType,
}

/// A value a predicate compares against.
///
/// It is exactly the stored [`AttributeValue`], so a predicate and the attribute it
/// matches speak one type system: the query's bytes are derived the same way the
/// writer derived the index's ([`AttributeValue::index_bytes`]), and a value of one
/// type never matches an attribute of another.
pub type AnnotVal = AttributeValue;

/// Where to start and how many to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    /// How many entities to return. Must be greater than zero.
    pub page_size: u64,
    /// Resume after this cursor, when continuing a previous page.
    pub cursor: Option<u64>,
}

/// The [`AuxiliaryStore`]'s answer to a query: the matching entity **keys** and
/// the work it took. The [`QueryProcessor`] turns the keys into full [`Entity`]
/// values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryMatches {
    /// The matching entity keys for this page.
    pub keys: Vec<EntityKey>,
    /// A cursor for the next page, if more remain.
    pub next_cursor: Option<u64>,
    /// The statistics of the work done.
    pub stats: QueryStats,
}

/// A page of query results: the matching entities and the work it took.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryResult {
    /// The matching entities for this page.
    pub entities: Vec<Entity>,
    /// A cursor for the next page, if more remain.
    pub next_cursor: Option<u64>,
    /// The statistics of the work done.
    pub stats: QueryStats,
}

/// What a query cost to run — for pricing, and for insight into a query's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryStats {
    /// Entities the query looked at.
    pub entities_scanned: u64,
    /// Entities it returned.
    pub entities_returned: u64,
    /// Index lookups it made.
    pub index_lookups: u64,
    /// Gas charged — a placeholder until [`CostModel`](crate::gas::CostModel) is
    /// filled in.
    pub gas_used: Gas,
    /// Whether it stopped early on a budget.
    pub partial: bool,
}

/// Answers a query with a page of entities.
///
/// It asks the [`AuxiliaryStore`] which entity keys match, then reads those
/// entities' bytes from the [`EntityStore`] and decodes them — adding paging and
/// statistics along the way.
pub trait QueryProcessor {
    /// The entity store it reads matched entities from.
    type Entities: EntityStore;
    /// The index it evaluates queries against.
    type Auxiliary: AuxiliaryStore;
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// Answer `query` at the current tip: one page of entities and the statistics
    /// of the work done.
    fn query(&self, query: &Query, page: PageParams) -> Result<QueryResult, Self::Error>;
}

/// Optional: answer queries against a **past block**, not just the current tip.
///
/// A host implements this on top of [`QueryProcessor`] only if it can serve
/// history. That requires history from both stores, so its [`Entities`] and
/// [`Auxiliary`] must be a [`HistoricalEntityStore`] and a
/// [`HistoricalAuxiliaryStore`]. The query runs exactly as
/// [`QueryProcessor::query`], but as of the state committed through block `at`.
///
/// [`Entities`]: QueryProcessor::Entities
/// [`Auxiliary`]: QueryProcessor::Auxiliary
pub trait HistoricalQuery: QueryProcessor
where
    Self::Entities: HistoricalEntityStore,
    Self::Auxiliary: HistoricalAuxiliaryStore,
{
    /// Answer `query` as of block `at`.
    fn query_at(
        &self,
        query: &Query,
        page: PageParams,
        at: BlockNumber,
    ) -> Result<QueryResult, Self::Error>;
}
