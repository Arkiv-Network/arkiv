//! The `arkiv_*` JSON-RPC error taxonomy.
//!
//! The codes are part of the frozen client surface: an SDK branches on the code,
//! not on the message. Each error also carries a machine-readable `data` object,
//! so a client can point at the offending character without scraping prose.
//!
//! | code | meaning |
//! |---|---|
//! | −32001 | parse error — malformed query text |
//! | −32002 | type error — a predicate that cannot be typed |
//! | −32003 | literal validation — a value that doesn't fit its tag |
//! | −32004 | limits exceeded — query too long, too many predicates, too deep |
//! | −32005 | cursor error — malformed, or bound to a different request |
//! | −32006 | block unavailable — outside the node's retained range |
//!
//! The parse-side codes come straight from
//! [`ParseErrorKind`](arkiv_query::ParseErrorKind), so the language and the wire
//! cannot drift: a new failure class in the parser has to pick a code there.

use arkiv_query::ParseError;
use jsonrpsee::types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};

/// A cursor that is malformed or bound to a different request.
pub const CURSOR_ERROR_CODE: i32 = -32005;
/// An `atBlock` outside the node's retained range.
pub const BLOCK_UNAVAILABLE_CODE: i32 = -32006;

/// Turn a query-language failure into its RPC error, preserving the position.
pub fn query_error(error: &ParseError) -> ErrorObjectOwned {
    let data = match error.position {
        Some(position) => serde_json::json!({
            "position": position,
            "message": error.message,
        }),
        None => serde_json::json!({ "message": error.message }),
    };
    ErrorObject::owned(error.kind.rpc_code(), error.message.clone(), Some(data))
}

/// A cursor this node will not resume from.
pub fn cursor_error(message: &str) -> ErrorObjectOwned {
    ErrorObject::owned(
        CURSOR_ERROR_CODE,
        message,
        Some(serde_json::json!({ "message": message })),
    )
}

/// An `atBlock` the node cannot answer for: ahead of the tip, or pruned.
pub fn block_unavailable(requested: u64, retained_tip: u64, reason: &str) -> ErrorObjectOwned {
    let message = format!("block {requested} is unavailable: {reason}");
    ErrorObject::owned(
        BLOCK_UNAVAILABLE_CODE,
        message.clone(),
        Some(serde_json::json!({
            "requested": requested,
            "latest": retained_tip,
            "message": message,
        })),
    )
}

/// A malformed request — the wrong shape, not the wrong query.
pub fn invalid_params(message: String) -> ErrorObjectOwned {
    ErrorObject::owned(INVALID_PARAMS_CODE, message, None::<()>)
}

/// Something went wrong inside the node.
pub fn internal_error(message: String) -> ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, message, None::<()>)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_for(query: &str) -> i32 {
        let error = arkiv_query::parse(query).unwrap_err();
        query_error(&error).code()
    }

    /// Each class of query failure reaches the client under its own code.
    #[test]
    fn query_failures_map_to_the_spec_codes() {
        assert_eq!(code_for("rank = = u256(1)"), -32001); // syntax
        assert_eq!(code_for("rank != u256(1)"), -32002); // type
        assert_eq!(code_for("rank = i32(2147483648)"), -32003); // literal
        assert_eq!(code_for(&"(".repeat(200)), -32004); // limits
    }

    #[test]
    fn a_query_error_carries_its_position() {
        let error = arkiv_query::parse("rank = 30").unwrap_err();
        let rpc = query_error(&error);
        let data: serde_json::Value =
            serde_json::from_str(rpc.data().unwrap().get()).expect("data is json");
        assert_eq!(data["position"], 7);
        assert!(data["message"].as_str().unwrap().contains("i32(…)"));
    }

    #[test]
    fn cursor_and_block_errors_have_their_own_codes() {
        assert_eq!(cursor_error("nope").code(), -32005);
        let block = block_unavailable(99, 10, "ahead of the chain tip");
        assert_eq!(block.code(), -32006);
        let data: serde_json::Value =
            serde_json::from_str(block.data().unwrap().get()).expect("data is json");
        assert_eq!(data["requested"], 99);
        assert_eq!(data["latest"], 10);
    }
}
