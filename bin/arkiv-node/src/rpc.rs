//! The `arkiv_*` JSON-RPC namespace.
//!
//! Methods are registered directly on a jsonrpsee [`RpcModule`] with native async
//! closures — no `#[rpc]` macro and no async-trait — and merged into reth's rpc
//! modules from `main`. Each read takes a fresh [`SnapshotAccountCode`] view of the
//! requested state (the tip, today).
//!
//! Here so far: `arkiv_getEntity`. `arkiv_query` and the count/timing methods join
//! once the query index's `evaluate` lands.

use alloy_primitives::{B256, hex};
use arkiv_interfaces::entity::{Attribute, Entity};
use arkiv_interfaces::state::EntityStore;
use arkiv_reth_entitystore::{CodeBackend, RethEntityStore};
use jsonrpsee::RpcModule;
use jsonrpsee::types::error::INTERNAL_ERROR_CODE;
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};
use reth_storage_api::StateProviderFactory;
use serde::Serialize;

use crate::snapshot::SnapshotAccountCode;

/// Build the `arkiv_*` [`RpcModule`], ready to merge into reth's rpc modules.
///
/// `provider` hands out a fresh state snapshot per call (`StateProviderFactory`).
pub fn arkiv_module<Provider>(provider: Provider) -> eyre::Result<RpcModule<()>>
where
    Provider: StateProviderFactory + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(());
    module.register_async_method("arkiv_getEntity", move |params, _ctx, _ext| {
        let provider = provider.clone();
        async move {
            let key: B256 = params
                .one()
                .map_err(|e| internal_error(format!("invalid params: {e}")))?;
            // The read hits MDBX, so run it off the async runtime.
            tokio::task::spawn_blocking(move || read_entity_at_tip(&provider, key))
                .await
                .map_err(|e| internal_error(format!("task join: {e}")))?
                .map_err(|e| internal_error(e.to_string()))
        }
    })?;
    Ok(module)
}

/// Read one entity by key from the latest committed state.
fn read_entity_at_tip<Provider>(provider: &Provider, key: B256) -> eyre::Result<Option<EntityView>>
where
    Provider: StateProviderFactory,
{
    let state = provider
        .latest()
        .map_err(|e| eyre::eyre!("latest state: {e:?}"))?;
    let mut store = RethEntityStore::new(CodeBackend::new(SnapshotAccountCode::new(state)));
    let entity = store
        .get(key.0)
        .map_err(|e| eyre::eyre!("get entity: {e:?}"))?;
    Ok(entity.map(EntityView::from_entity))
}

/// The JSON shape of an entity: byte fields as `0x`-hex, text fields as strings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityView {
    pub key: String,
    pub owner: String,
    pub creator: String,
    pub created_at_block: u64,
    pub last_modified_at_block: u64,
    pub expires_at: u64,
    pub content_type: String,
    pub payload: String,
    pub attributes: Vec<AttributeView>,
}

/// The JSON shape of one attribute.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttributeView {
    pub key: String,
    pub value_type: u8,
    pub value: String,
}

impl EntityView {
    fn from_entity(entity: Entity) -> Self {
        Self {
            key: hex_prefixed(&entity.key),
            owner: hex_prefixed(&entity.owner),
            creator: hex_prefixed(&entity.creator),
            created_at_block: entity.created_at_block,
            last_modified_at_block: entity.last_modified_at_block,
            expires_at: entity.expires_at,
            content_type: String::from_utf8_lossy(&entity.content_type).into_owned(),
            payload: hex_prefixed(&entity.payload),
            attributes: entity
                .attributes
                .into_iter()
                .map(AttributeView::from_attribute)
                .collect(),
        }
    }
}

impl AttributeView {
    fn from_attribute(attribute: Attribute) -> Self {
        Self {
            key: String::from_utf8_lossy(&attribute.key).into_owned(),
            value_type: attribute.value_type,
            value: hex_prefixed(&attribute.value),
        }
    }
}

fn hex_prefixed(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn internal_error(message: String) -> ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, message, None::<()>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::entity::ATTR_STRING;

    #[test]
    fn entity_view_projects_bytes_as_hex_and_text_as_strings() {
        let entity = Entity {
            key: [0x11; 32],
            owner: [0x22; 20],
            creator: [0x33; 20],
            created_at_block: 5,
            last_modified_at_block: 6,
            expires_at: 100,
            content_type: b"text/plain".to_vec(),
            payload: vec![0xDE, 0xAD],
            attributes: vec![Attribute {
                key: b"color".to_vec(),
                value_type: ATTR_STRING,
                value: b"blue".to_vec(),
            }],
        };
        let view = EntityView::from_entity(entity);
        assert_eq!(view.key, format!("0x{}", "11".repeat(32)));
        assert_eq!(view.owner, format!("0x{}", "22".repeat(20)));
        assert_eq!(view.content_type, "text/plain");
        assert_eq!(view.payload, "0xdead");
        assert_eq!(view.expires_at, 100);
        assert_eq!(view.attributes[0].key, "color");
        assert_eq!(view.attributes[0].value, "0x626c7565"); // "blue"
    }
}
