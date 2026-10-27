//! The entity state the chain should hold, built from the effects of included
//! transactions in block order, with expiries resolved at their block.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256};
use serde_json::{Value, json};

use crate::plan::Effect;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    pub owner: Address,
    pub expires_at: u64,
    pub attrs: BTreeMap<String, String>,
}

impl Expected {
    pub fn to_json(&self) -> Value {
        json!({
            "owner": format!("{:#x}", self.owner),
            "expiresAt": self.expires_at,
            "attributes": self.attrs,
        })
    }
}

#[derive(Default)]
pub struct Model {
    /// `None` once deleted.
    entities: BTreeMap<B256, Option<Expected>>,
}

impl Model {
    /// Apply a successful transaction by `sender`, included in `block`, and
    /// return the keys it touched.
    pub fn apply(&mut self, sender: Address, block: u64, effects: &[Effect]) -> BTreeSet<B256> {
        let mut touched = BTreeSet::new();
        for effect in effects {
            touched.insert(effect.key());
            match effect {
                Effect::Create {
                    key,
                    attrs,
                    lifetime,
                } => {
                    self.entities.insert(
                        *key,
                        Some(Expected {
                            owner: sender,
                            expires_at: block + lifetime,
                            attrs: attrs.clone(),
                        }),
                    );
                }
                Effect::Set { key, name, value } => {
                    self.live(key).attrs.insert(name.clone(), value.clone());
                }
                Effect::Unset { key, name } => {
                    self.live(key).attrs.remove(name);
                }
                Effect::Extend { key, min_lifetime } => {
                    self.live(key).expires_at = block + min_lifetime;
                }
                Effect::Transfer { key, to } => {
                    self.live(key).owner = *to;
                }
                Effect::Delete { key } => {
                    self.entities.insert(*key, None);
                }
            }
        }
        touched
    }

    fn live(&mut self, key: &B256) -> &mut Expected {
        self.entities
            .get_mut(key)
            .and_then(Option::as_mut)
            .unwrap_or_else(|| panic!("the plan only touches live entities, not {key}"))
    }

    pub fn get(&self, key: &B256) -> Option<&Option<Expected>> {
        self.entities.get(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &B256> {
        self.entities.keys()
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }
}

/// How `actual`, an `arkiv_getEntity` answer, differs from `expected`, or
/// `None` when it matches. Only user attributes are compared.
pub fn mismatch(expected: &Option<Expected>, actual: &Value) -> Option<String> {
    let Some(expected) = expected else {
        return (!actual.is_null()).then(|| "deleted, but the node still has it".into());
    };
    if actual.is_null() {
        return Some("the node has no such entity".into());
    }
    let mut differences = Vec::new();
    let owner = actual["owner"].as_str().unwrap_or_default().to_lowercase();
    if owner != format!("{:#x}", expected.owner) {
        differences.push(format!("owner {owner}"));
    }
    let expires_at = actual["expiresAt"]
        .as_str()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    if expires_at != Some(expected.expires_at) {
        differences.push(format!("expiresAt {:?}", actual["expiresAt"]));
    }
    let attrs: BTreeMap<String, String> = actual["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let name = a["name"].as_str()?;
            (!name.starts_with('$')).then(|| {
                let value = a["value"]
                    .as_str()
                    .map_or_else(|| a["value"].to_string(), ToOwned::to_owned);
                (name.to_string(), value)
            })
        })
        .collect();
    if attrs != expected.attrs {
        differences.push(format!("attributes {attrs:?}"));
    }
    (!differences.is_empty()).then(|| differences.join("; "))
}
