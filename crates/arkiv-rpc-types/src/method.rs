//! Per-method request and response shapes.
//!
//! Chain quantities go over as hex strings like the `eth_*` namespace, since a
//! block number can exceed what a JSON number holds exactly. Both spellings are
//! accepted on the way in — see [`de_u64_flexible`](crate::entity::de_u64_flexible()).

use alloy_eips::BlockNumberOrTag;
use serde::{Deserialize, Serialize};

use crate::entity::{EntityData, Select, de_u64_flexible, ser_u64_hex};

/// Page size served when an `arkiv_query` request omits `limit`.
pub const DEFAULT_PAGE_SIZE: u64 = 100;

/// The largest `limit` a node will serve. Part of the client contract, so an SDK
/// can bound its own paging instead of discovering the ceiling by being rejected.
pub const MAX_PAGE_SIZE: u64 = 200;

/// The `arkiv_query` options — the second positional param. The full call is
/// `["<query>", { "atBlock": …, "select": …, "limit": …, "cursor": … }]`; the
/// object and every field in it are optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryOptions {
    /// Block to evaluate against. `None` / `"latest"` reads head state; a hex
    /// number reads historical state, and must be within the retained range.
    pub at_block: Option<BlockNumberOrTag>,
    /// Fields to return. Absent means the key alone.
    pub select: Option<Select>,
    /// Page size, hex or JSON number. Resolved by [`resolve_limit`].
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub limit: Option<u64>,
    /// Opaque cursor from the previous page, bound to its query, block and
    /// projection. Clients must not parse it.
    pub cursor: Option<String>,
}

/// The page size to serve. Over the ceiling is an error rather than a trim —
/// silently serving 200 of a requested 500 reads as a short page, so a caller
/// would stop early. `Err` carries the node's invalid-params message.
pub fn resolve_limit(requested: Option<u64>) -> Result<u64, String> {
    match requested {
        None => Ok(DEFAULT_PAGE_SIZE),
        Some(0) => Err("limit must be at least 1".to_string()),
        Some(limit) if limit > MAX_PAGE_SIZE => Err(format!(
            "limit {limit} exceeds the node maximum of {MAX_PAGE_SIZE}"
        )),
        Some(limit) => Ok(limit),
    }
}

/// The `arkiv_getEntityCount` request: an optional query to filter by (default
/// `$all`) and an optional past block (default the tip).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub block: Option<u64>,
}

/// The `arkiv_query` response: one page of matched entities, the block the
/// query evaluated against (hex), an opaque continuation cursor (absent on
/// the last page), and how many key-value pairs the node read to answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<EntityData>,
    #[serde(serialize_with = "ser_u64_hex", deserialize_with = "de_u64_required")]
    pub block_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Trie nodes looked up in the node store while evaluating the query and
    /// reading the page's entities. A measure of the work, not a price.
    #[serde(default)]
    pub nodes_read: u64,
    /// Index entries visited by the predicate walks: each predicate costs the
    /// number of entities that satisfy it.
    #[serde(default)]
    pub index_entries_scanned: u64,
    /// Elements consumed by the `AND`, `OR` and `NOT` merges.
    #[serde(default)]
    pub merge_steps: u64,
}

/// The `arkiv_getBlockTiming` response.
///
/// Serialized in `snake_case` — the shape the `arkiv-cli` `block-timing` command
/// deserializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTimingView {
    /// The chain tip's block number.
    pub current_block: u64,
    /// The tip block's timestamp (unix seconds).
    pub current_block_time: u64,
    /// Seconds between the tip and its predecessor (`0` at genesis).
    pub duration: u64,
}

/// A required chain quantity, in either spelling.
fn de_u64_required<'de, D>(de: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    de_u64_flexible(de)?.ok_or_else(|| D::Error::custom("expected a quantity, found null"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_limit_gets_the_default_page() {
        assert_eq!(resolve_limit(None).unwrap(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn a_limit_outside_the_range_is_refused_not_clamped() {
        assert!(resolve_limit(Some(0)).is_err());
        assert!(resolve_limit(Some(MAX_PAGE_SIZE + 1)).is_err());
        assert_eq!(resolve_limit(Some(MAX_PAGE_SIZE)).unwrap(), MAX_PAGE_SIZE);
        assert_eq!(resolve_limit(Some(1)).unwrap(), 1);
    }

    #[test]
    fn a_query_response_round_trips_through_json() {
        let response = QueryResponse {
            data: vec![],
            block_number: 0x8e1ff,
            cursor: Some("b64:abc".to_string()),
            nodes_read: 17,
            index_entries_scanned: 40,
            merge_steps: 45,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["blockNumber"], "0x8e1ff", "quantities go over as hex");
        assert_eq!(json["nodesRead"], 17);
        assert_eq!(json["indexEntriesScanned"], 40);
        assert_eq!(json["mergeSteps"], 45);
        let back: QueryResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back.block_number, 0x8e1ff);
        assert_eq!(back.cursor.as_deref(), Some("b64:abc"));
        assert_eq!(back.nodes_read, 17);
    }

    /// The last page omits `cursor` entirely rather than sending null.
    #[test]
    fn a_final_page_carries_no_cursor_field() {
        let json = serde_json::to_value(QueryResponse {
            data: vec![],
            block_number: 1,
            cursor: None,
            nodes_read: 0,
            index_entries_scanned: 0,
            merge_steps: 0,
        })
        .unwrap();
        assert!(json.get("cursor").is_none());
        let back: QueryResponse = serde_json::from_value(json).unwrap();
        assert!(back.cursor.is_none());
        // Older responses without the field still parse.
        let old: QueryResponse =
            serde_json::from_value(serde_json::json!({ "data": [], "blockNumber": "0x1" }))
                .unwrap();
        assert_eq!(old.nodes_read, 0);
    }

    #[test]
    fn query_options_accept_a_limit_in_either_spelling() {
        let hex: QueryOptions =
            serde_json::from_value(serde_json::json!({ "limit": "0x64" })).unwrap();
        let number: QueryOptions =
            serde_json::from_value(serde_json::json!({ "limit": 100 })).unwrap();
        assert_eq!(hex.limit, Some(100));
        assert_eq!(number.limit, Some(100));
    }

    #[test]
    fn an_unknown_query_option_is_rejected() {
        let bad = serde_json::from_value::<QueryOptions>(serde_json::json!({ "atBlok": "latest" }));
        assert!(bad.is_err());
    }
}
