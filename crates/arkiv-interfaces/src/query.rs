//! Queries: the query language, and the trait that answers it.

mod filter;
mod order;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::entity::{AttributeType, AttributeValue, Entity};
use crate::primitives::{EntityAddress, Gas};
use crate::statemanager::{BlockRef, StateView};

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
/// "attribute is set with this type, and differs" is
/// [`Exists`](Self::Exists) minus an [`Eq`](Self::Eq), which the language has
/// no single operator for; `NOT (attr = value)` gives the wider complement
/// explicitly.
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
    /// Matches entities whose `key` lies within `bounds`, inclusive at both
    /// ends — the `BETWEEN` operator.
    Between { key: AnnotKey, bounds: ValueRange },
    /// Matches entities whose `key` starts with `value` — the `STARTSWITH`
    /// operator, a raw UTF-8 byte prefix with no normalization.
    StartsWith { key: AnnotKey, value: AnnotVal },
    /// Matches entities that carry the user attribute `name` as an **indexed**
    /// attribute, without comparing its value — the `exists(name)` and
    /// `typeof(name) = tag` operators.
    ///
    /// A name rather than an [`AnnotKey`]: every entity carries the system
    /// attributes, so the language asks this of user attributes only.
    Exists {
        name: String,
        attr_type: Option<AttributeType>,
    },
    /// Matches entities satisfying every subquery. A chain of one connective
    /// is one node, `a AND b AND c` an `And` of three, so a node is only as
    /// deep as the query nests; the parser builds it with two or more.
    And(Vec<Query>),
    /// Matches entities satisfying some subquery. One node per chain, as
    /// [`And`](Self::And).
    Or(Vec<Query>),
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
/// Each is stored under its [`annotations`](crate::entity::annotations) constant,
/// which carries the same name: the language and the store spell every system
/// attribute alike. What this enum adds is which of them are queryable at all.
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
}

/// A value a predicate compares against.
///
/// It is exactly the stored [`AttributeValue`], so a predicate and the attribute it
/// matches speak one type system: the query's bytes are derived the same way the
/// writer derived the index's ([`AttributeValue::index_bytes`]), and a value of one
/// type never matches an attribute of another.
pub type AnnotVal = AttributeValue;

/// The bounds of a [`Query::Between`]: a low and a high value of one type,
/// both included. A `low` above `high` admits no value, and matches nothing.
///
/// The fields are private, so bounds of two types cannot be built: the
/// language makes them a type error, and no attribute could hold both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueRange {
    low: AnnotVal,
    high: AnnotVal,
}

impl ValueRange {
    /// `low` to `high`, or `None` when the two are not of one type.
    pub fn new(low: AnnotVal, high: AnnotVal) -> Option<Self> {
        (low.attr_type() == high.attr_type()).then_some(Self { low, high })
    }

    /// The lower bound, included.
    pub fn low(&self) -> &AnnotVal {
        &self.low
    }

    /// The upper bound, included.
    pub fn high(&self) -> &AnnotVal {
        &self.high
    }
}

/// A whole query string: the [`Query`] that selects entities, and the order to
/// return them in — `<query> [ORDER BY …]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub filter: Query,
    /// Absent, matches come in the store's own order.
    pub order: Option<OrderBy>,
}

/// `ORDER BY key:attr_type [ASC | DESC]`: by one attribute, read as one
/// type.
///
/// An entity without the attribute, or holding another type there, has no
/// value to sort by: it sorts below every value, so first ascending and last
/// descending. Entities that tie come in the store's own order, in either
/// direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub key: AnnotKey,
    pub attr_type: AttributeType,
    pub direction: Direction,
}

/// Which way an [`OrderBy`] runs. `ASC` when the query names neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Ascending,
    Descending,
}

/// Where to start and how many to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    /// How many entities to return. Must be greater than zero.
    pub page_size: u64,
    /// Resume after this cursor, when continuing a previous page.
    pub cursor: Option<u64>,
}

/// The key half of an answer: the matching entity **keys** and the work it
/// took. The [`QueryProcessor`] builds one of these from the index lanes, then
/// turns the keys into full [`Entity`] values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryMatches {
    /// The matching entity keys for this page.
    pub keys: Vec<EntityAddress>,
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
/// It decomposes the query tree into the index lanes' primitives —
/// [`get_equal_entities`](crate::statemanager::EqualityIndexStore::get_equal_entities),
/// [`get_within_range`](crate::statemanager::RangeIndexStore::get_within_range),
/// [`get_prefixed_entities`](crate::statemanager::EqualityIndexStore::get_prefixed_entities)
/// — combines the matches (`AND`/`OR`/`NOT`), then reads the matching entities
/// back — adding paging and statistics along the way. One [`StateView`] for all
/// of it: the indexes and the entities are lanes of the same view, so a
/// processor never holds two stores that could disagree about which block they
/// are at.
pub trait QueryProcessor {
    /// The view this processor reads the indexes and the entities through.
    type State: StateView;
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// Answer `query` at the current tip: one page of entities and the statistics
    /// of the work done.
    fn query(&self, query: &Query, page: PageParams) -> Result<QueryResult, Self::Error>;
}

/// Optional: answer queries against a **past block**, not just the current tip.
///
/// History is served by opening a [`StateView`] at the older block, so this is
/// implementable exactly as far back as the backend retains the index and
/// entity lanes ([`has_store`](StateView::has_store)) — a block outside that
/// window is an error, never a silently-wrong answer from the tip. The query
/// runs exactly as [`QueryProcessor::query`], but as of the state committed
/// through `at`.
pub trait HistoricalQuery: QueryProcessor {
    /// Answer `query` as of block `at`.
    fn query_at(
        &self,
        query: &Query,
        page: PageParams,
        at: BlockRef,
    ) -> Result<QueryResult, Self::Error>;
}
