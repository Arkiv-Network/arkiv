//! Failures, as jsonrpsee error objects.
//!
//! The **codes** are the client's contract and are defined in
//! [`arkiv_rpc_types::error`], which is also where the table of what each one
//! means lives; the parse-side codes come from that crate's authority in turn,
//! [`ParseErrorKind`](arkiv_query::ParseErrorKind), so the language and the wire
//! cannot drift. This module is only the server half: turning each failure into
//! the [`ErrorObjectOwned`] jsonrpsee puts on the wire, with the machine-readable
//! `data` object a client reads instead of scraping the message.

use arkiv_query::ParseError;
use jsonrpsee::types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};

pub use arkiv_rpc_types::error::{BLOCK_UNAVAILABLE_CODE, CURSOR_ERROR_CODE};

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
        //                                  0123456789012345
        let error = arkiv_query::parse("$createdAt >= 1").unwrap_err();
        let rpc = query_error(&error);
        let data: serde_json::Value =
            serde_json::from_str(rpc.data().unwrap().get()).expect("data is json");
        assert_eq!(data["position"], 14, "points at the offending literal");
        assert!(data["message"].as_str().unwrap().contains("u64"));
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
