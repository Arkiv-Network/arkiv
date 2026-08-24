//! Bounds on how big a query may be.
//!
//! [`parse`](fn@crate::parse) runs on whatever an unauthenticated RPC caller sends,
//! and both the parser and the evaluator walk the query tree recursively — so an
//! unbounded query is an unbounded stack. These caps make the work a query can
//! ask for finite before any of it is done. Exceeding one is a
//! [`QueryLimitError`](crate::ParseErrorKind::QueryLimitError), which a node
//! reports as
//! [`rpc_error_codes::QUERY_LIMIT`](arkiv_interfaces::constants::rpc_error_codes::QUERY_LIMIT).
//!
//! The numbers below are policy, not protocol: they bound a node's own work and
//! can be retuned without changing what any query *means*. The one exception is
//! [`MAX_ATTRIBUTE_NAME_BYTES`], re-exported from
//! [`arkiv_interfaces::constants`] — a name the engine would reject is not a
//! query this node should have parsed.

/// Longest query string accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 8 * 1024;

/// Most predicates one query may combine.
pub const MAX_PREDICATES: usize = 64;

/// Deepest nesting of parentheses and `NOT`. Bounds parser and evaluator
/// recursion together, since the evaluator walks the tree the parser builds.
pub const MAX_NESTING_DEPTH: usize = 32;

/// Longest attribute name, in bytes — the width the ABI's `Ident32` carries.
pub use arkiv_interfaces::constants::MAX_ATTRIBUTE_NAME_BYTES;
