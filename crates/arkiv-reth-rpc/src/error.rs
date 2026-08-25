//! Failures, as jsonrpsee error objects. The codes live in
//! [`rpc_error_codes`] and the `data` each one carries is tabulated in
//! [`arkiv_rpc_types`]; this is the server half that builds the wire object.

use arkiv_query::ParseError;
use jsonrpsee::types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};

pub use arkiv_interfaces::constants::rpc_error_codes;

/// Turn a query-language failure into its RPC error, preserving the position.
pub fn query_error(error: &ParseError) -> ErrorObjectOwned {
    let data = match error.failure_position {
        // `position` on the wire: the key is part of the frozen contract, so it
        // does not follow the Rust field's name.
        Some(failure_position) => serde_json::json!({
            "position": failure_position,
            "message": error.message,
        }),
        None => serde_json::json!({ "message": error.message }),
    };
    ErrorObject::owned(error.kind.rpc_code(), error.message.clone(), Some(data))
}

/// A cursor this node will not resume from.
pub fn cursor_error(message: &str) -> ErrorObjectOwned {
    ErrorObject::owned(
        rpc_error_codes::CURSOR_ERROR,
        message,
        Some(serde_json::json!({ "message": message })),
    )
}

/// An `atBlock` the node cannot answer for: ahead of the tip, or pruned.
pub fn block_unavailable(requested: u64, retained_tip: u64, reason: &str) -> ErrorObjectOwned {
    let message = format!("block {requested} is unavailable: {reason}");
    ErrorObject::owned(
        rpc_error_codes::BLOCK_UNAVAILABLE,
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
        assert_eq!(
            code_for("rank = = u256(1)"),
            rpc_error_codes::MALFORMED_INPUT
        );
        assert_eq!(code_for("rank != u256(1)"), rpc_error_codes::TYPE_ERROR);
        assert_eq!(
            code_for("rank = i32(2147483648)"),
            rpc_error_codes::LITERAL_ERROR
        );
        assert_eq!(code_for(&"(".repeat(200)), rpc_error_codes::QUERY_LIMIT);
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
        assert_eq!(cursor_error("nope").code(), rpc_error_codes::CURSOR_ERROR);
        let block = block_unavailable(99, 10, "ahead of the chain tip");
        assert_eq!(block.code(), rpc_error_codes::BLOCK_UNAVAILABLE);
        let data: serde_json::Value =
            serde_json::from_str(block.data().unwrap().get()).expect("data is json");
        assert_eq!(data["requested"], 99);
        assert_eq!(data["latest"], 10);
    }
}
