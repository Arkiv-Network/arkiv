//! Bounds on how big a query may be.
//!
//! [`parse`](crate::parse) runs on whatever an unauthenticated RPC caller sends,
//! and both the parser and the evaluator walk the query tree recursively — so an
//! unbounded query is an unbounded stack. These caps make the work a query can
//! ask for finite before any of it is done. Exceeding one is a
//! [`Limit`](crate::ParseErrorKind::Limit) error, which a node reports as
//! `-32004`.
//!
//! The numbers are policy, not protocol: they bound a node's own work and can be
//! retuned without changing what any query *means*.

/// Longest query string accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 8 * 1024;

/// Most predicates one query may combine.
pub const MAX_PREDICATES: usize = 64;

/// Deepest nesting of parentheses and `NOT`. Bounds parser and evaluator
/// recursion together, since the evaluator walks the tree the parser builds.
pub const MAX_NESTING_DEPTH: usize = 32;

/// Longest attribute name, in bytes — the width the ABI's `Ident32` carries.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 32;
