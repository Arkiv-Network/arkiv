//! The `arkiv_*` error codes. Frozen: an SDK branches on the code, not the message.
//!
//! | code | meaning | `data` |
//! |---|---|---|
//! | −32001 | parse error — malformed query text | `position`, `message` |
//! | −32002 | type error — a predicate that cannot be typed | `position`, `message` |
//! | −32003 | literal validation — a value that doesn't fit its tag | `position`, `message` |
//! | −32004 | limits exceeded — query too long, too deep, too many predicates | `message` |
//! | −32005 | cursor error — malformed, or bound to a different request | `message` |
//! | −32006 | block unavailable — outside the node's retained range | `requested`, `latest`, `message` |
//!
//! The first four are owned by `ParseErrorKind::rpc_code` in `arkiv-query`, so a
//! new parser failure class has to pick its code there. The last two are the
//! node's own. `position` is a byte offset into the query text.

/// A cursor that is malformed or bound to a different request.
pub const CURSOR_ERROR_CODE: i32 = -32005;

/// An `atBlock` outside the node's retained range.
pub const BLOCK_UNAVAILABLE_CODE: i32 = -32006;
