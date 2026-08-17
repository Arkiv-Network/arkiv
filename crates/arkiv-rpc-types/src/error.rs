//! The `arkiv_*` JSON-RPC error taxonomy.
//!
//! The codes are part of the frozen client surface: an SDK branches on the code,
//! not on the message. Each error also carries a machine-readable `data` object,
//! so a client can point at the offending character without scraping prose.
//!
//! | code | meaning | `data` |
//! |---|---|---|
//! | −32001 | parse error — malformed query text | `position`, `message` |
//! | −32002 | type error — a predicate that cannot be typed | `position`, `message` |
//! | −32003 | literal validation — a value that doesn't fit its tag | `position`, `message` |
//! | −32004 | limits exceeded — query too long, too many predicates, too deep | `message` |
//! | −32005 | cursor error — malformed, or bound to a different request | `message` |
//! | −32006 | block unavailable — outside the node's retained range | `requested`, `latest`, `message` |
//!
//! The first four are the query language's, and are **owned by the parser**:
//! [`ParseErrorKind::rpc_code`](https://docs.rs/arkiv-query) is where each class
//! picks its number, so a new failure class in the parser has to choose a code
//! there and the language cannot drift from the wire. The last two are the
//! node's own and are named here.
//!
//! `position` is a byte offset into the query text, so a client can underline the
//! exact character rather than re-parsing to guess.

/// A cursor that is malformed or bound to a different request.
pub const CURSOR_ERROR_CODE: i32 = -32005;

/// An `atBlock` outside the node's retained range.
pub const BLOCK_UNAVAILABLE_CODE: i32 = -32006;
