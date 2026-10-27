//! The run's transactions, planned up front, one sender at a time.
//!
//! Every entity is touched only by the sender that created it: a transfer
//! hands it to [`SINK`], which never sends, so it leaves the run. A sender's
//! transactions land in its nonce order, so planning them in that order with
//! the sender's own view of its entities gives every outcome exactly, whatever
//! the other senders do and however the blocks interleave them. The one thing
//! that depends on inclusion, an expiry, is relative here and resolved by the
//! model when the block is known.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolCall;
use arkiv_bindings::{Attribute, AttributeValue, IEntityRegistry, Ident32, Operation};
use arkiv_harness::{DEV_CHAIN_ID, EntityCreationNonce, derive_entity_address};
use serde_json::{Value, json};

use crate::rng::Rng;

/// Where transfers send entities and value: an address no test key controls.
pub const SINK: Address = Address::repeat_byte(0x5e);

/// The gas limit of an entity transaction: ample for the small batches here,
/// and low enough that many fit a block.
pub const ENTITY_GAS: u64 = 500_000;
/// A mined view call is charged its flat 21,000 plus calldata.
pub const VIEW_GAS: u64 = 60_000;
pub const TRANSFER_GAS: u64 = 21_000;
/// Below any transaction's intrinsic gas: the pool refuses it at ingress.
pub const BELOW_INTRINSIC_GAS: u64 = 20_000;

/// Lifetimes far beyond any run, so no entity expires while it is planned on.
const LIFETIME: (u64, u64) = (50_000, 60_000);
/// An extend's relative lifetime starts above every create's and grows with
/// each extend of the entity, so each lands strictly later than the last.
const EXTEND_FLOOR: u64 = 100_000;

/// User attribute names, each always indexed or always not, all strings.
const NAMES: [(&str, bool); 5] = [
    ("color", true),
    ("label", false),
    ("note", false),
    ("size", false),
    ("tag", true),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Success,
    /// A business-rule revert: the batch is rolled back whole.
    Revert,
    /// Out of gas: the batch is rolled back and pays all it supplied.
    OutOfGas,
    /// The pool refuses the transaction; it never gets a place in a block.
    RejectedAtIngress,
}

impl Expect {
    pub fn name(self) -> &'static str {
        match self {
            Expect::Success => "success",
            Expect::Revert => "revert",
            Expect::OutOfGas => "out_of_gas",
            Expect::RejectedAtIngress => "rejected_at_ingress",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Body {
    /// `execute(Operation[])` on `ARKIV_ADDRESS`.
    Execute(Vec<Operation>),
    /// A plain value transfer to [`SINK`].
    Transfer { value: U256 },
    /// The `entityNonce` view, mined as a transaction.
    EntityNonce,
}

/// What one op asks of one entity: what it does, when its transaction
/// succeeds.
#[derive(Clone, Debug)]
pub enum Effect {
    Create {
        key: B256,
        attrs: BTreeMap<String, String>,
        lifetime: u64,
    },
    Set {
        key: B256,
        name: String,
        value: String,
    },
    Unset {
        key: B256,
        name: String,
    },
    Extend {
        key: B256,
        min_lifetime: u64,
    },
    Transfer {
        key: B256,
        to: Address,
    },
    Delete {
        key: B256,
    },
}

impl Effect {
    /// The op, readable, for the run's transaction list.
    pub fn to_json(&self) -> Value {
        let key = format!("{:#x}", self.key());
        match self {
            Effect::Create {
                attrs, lifetime, ..
            } => json!({ "op": "create", "key": key, "lifetime": lifetime, "attributes": attrs }),
            Effect::Set { name, value, .. } => {
                json!({ "op": "patch", "key": key, "set": { name: value } })
            }
            Effect::Unset { name, .. } => json!({ "op": "patch", "key": key, "unset": name }),
            Effect::Extend { min_lifetime, .. } => {
                json!({ "op": "extend", "key": key, "minLifetime": min_lifetime })
            }
            Effect::Transfer { to, .. } => {
                json!({ "op": "transfer", "key": key, "to": format!("{to:#x}") })
            }
            Effect::Delete { .. } => json!({ "op": "delete", "key": key }),
        }
    }

    pub fn key(&self) -> B256 {
        match self {
            Effect::Create { key, .. }
            | Effect::Set { key, .. }
            | Effect::Unset { key, .. }
            | Effect::Extend { key, .. }
            | Effect::Transfer { key, .. }
            | Effect::Delete { key } => *key,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlannedTx {
    /// What the transaction is, for the report: `create x2`, `patch of a
    /// deleted entity`, …
    pub label: String,
    pub body: Body,
    pub gas: u64,
    pub expect: Expect,
    /// What each op asks for, in order, for the report; the model applies
    /// them only to a transaction expected to succeed.
    pub effects: Vec<Effect>,
}

/// One of the sender's entities, as the sender knows it.
#[derive(Clone)]
enum Local {
    Live {
        attrs: BTreeMap<String, String>,
        extend_floor: u64,
    },
    Deleted,
    /// Transferred to [`SINK`]: still there, no longer the sender's.
    Given,
}

/// Plan `count` transactions for `sender`, the run's `index`-th.
pub fn plan_sender(seed: u64, index: u64, sender: Address, count: usize) -> Vec<PlannedTx> {
    let mut planner = Planner {
        rng: Rng::new(seed, index),
        sender,
        minted: 0,
        entities: BTreeMap::new(),
    };
    (0..count).map(|_| planner.next()).collect()
}

struct Planner {
    rng: Rng,
    sender: Address,
    /// The sender's minting nonce once everything planned so far has landed.
    minted: u64,
    entities: BTreeMap<B256, Local>,
}

impl Planner {
    fn next(&mut self) -> PlannedTx {
        let live = self.live_keys();
        let roll = self.rng.below(100);
        match roll {
            _ if live.is_empty() && (25..65).contains(&roll) => self.create(),
            0..25 => self.create(),
            25..45 => self.patch(&live),
            45..55 => self.extend(&live),
            55..60 => self.give_away(&live),
            60..65 => self.delete(&live),
            65..75 => PlannedTx {
                label: "value transfer".into(),
                body: Body::Transfer {
                    value: U256::from(self.rng.range(1, 1_000_000_000)),
                },
                gas: TRANSFER_GAS,
                expect: Expect::Success,
                effects: Vec::new(),
            },
            75..80 => PlannedTx {
                label: "mined view call".into(),
                body: Body::EntityNonce,
                gas: VIEW_GAS,
                expect: Expect::Success,
                effects: Vec::new(),
            },
            80..88 => self.deliberate_revert(),
            88..94 => {
                let (op, effect) = self.create_op(self.minted);
                let ops = vec![op];
                let gas = intrinsic_gas(&execute_calldata(&ops)) + 1_000;
                PlannedTx {
                    label: "create out of gas".into(),
                    body: Body::Execute(ops),
                    gas,
                    expect: Expect::OutOfGas,
                    effects: vec![effect],
                }
            }
            _ => {
                let (op, effect) = self.create_op(self.minted);
                PlannedTx {
                    label: "create below intrinsic gas".into(),
                    body: Body::Execute(vec![op]),
                    gas: BELOW_INTRINSIC_GAS,
                    expect: Expect::RejectedAtIngress,
                    effects: vec![effect],
                }
            }
        }
    }

    fn live_keys(&self) -> Vec<B256> {
        self.entities
            .iter()
            .filter(|(_, e)| matches!(e, Local::Live { .. }))
            .map(|(k, _)| *k)
            .collect()
    }

    fn key(&self, nonce: u64) -> B256 {
        B256::from(derive_entity_address(
            DEV_CHAIN_ID,
            &self.sender.into_array(),
            EntityCreationNonce::new(nonce),
            0,
        ))
    }

    fn value(&mut self) -> String {
        format!("v{}", self.rng.below(1_000_000))
    }

    /// A create minting from `nonce`, with its effect.
    fn create_op(&mut self, nonce: u64) -> (Operation, Effect) {
        let key = self.key(nonce);
        let lifetime = self.rng.range(LIFETIME.0, LIFETIME.1);
        let mut attrs = BTreeMap::new();
        for _ in 0..self.rng.range(1, 3) {
            let (name, _) = *self.rng.pick(&NAMES);
            let value = self.value();
            attrs.insert(name.to_string(), value);
        }
        let op = Operation::create(0, 0, lifetime, 0, attrs.iter().map(set_attr).collect());
        (
            op,
            Effect::Create {
                key,
                attrs,
                lifetime,
            },
        )
    }

    /// One or two creates, the first sometimes patched in the same batch.
    fn create(&mut self) -> PlannedTx {
        let count = self.rng.range(1, 2);
        let mut ops = Vec::new();
        let mut effects = Vec::new();
        for i in 0..count {
            let (op, effect) = self.create_op(self.minted + i);
            ops.push(op);
            effects.push(effect);
        }
        let mut label = format!("create x{count}");
        if self.rng.chance(30) {
            let key = effects[0].key();
            let (name, _) = *self.rng.pick(&NAMES);
            let value = self.value();
            ops.push(Operation::patch(
                key,
                vec![set_attr((&name.to_string(), &value))],
            ));
            effects.push(Effect::Set {
                key,
                name: name.into(),
                value,
            });
            label.push_str(" + patch");
        }
        self.minted += count;
        self.commit(&effects);
        PlannedTx {
            label,
            body: Body::Execute(ops),
            gas: ENTITY_GAS,
            expect: Expect::Success,
            effects,
        }
    }

    fn patch(&mut self, live: &[B256]) -> PlannedTx {
        let key = *self.rng.pick(live);
        let Some(Local::Live { attrs, .. }) = self.entities.get(&key) else {
            unreachable!("picked from the live keys")
        };
        let existing: Vec<String> = attrs.keys().cloned().collect();
        let (mutation, effect, label) = if !existing.is_empty() && self.rng.chance(30) {
            let name = self.rng.pick(&existing).clone();
            (
                Attribute::tombstone(Ident32::encode(&name).unwrap()),
                Effect::Unset {
                    key,
                    name: name.clone(),
                },
                "patch (unset)",
            )
        } else {
            let (name, _) = *self.rng.pick(&NAMES);
            let value = self.value();
            (
                set_attr((&name.to_string(), &value)),
                Effect::Set {
                    key,
                    name: name.into(),
                    value,
                },
                "patch (set)",
            )
        };
        let effects = vec![effect];
        self.commit(&effects);
        PlannedTx {
            label: label.into(),
            body: Body::Execute(vec![Operation::patch(key, vec![mutation])]),
            gas: ENTITY_GAS,
            expect: Expect::Success,
            effects,
        }
    }

    fn extend(&mut self, live: &[B256]) -> PlannedTx {
        let key = *self.rng.pick(live);
        let step = self.rng.range(1, 1_000);
        let Some(Local::Live { extend_floor, .. }) = self.entities.get_mut(&key) else {
            unreachable!("picked from the live keys")
        };
        *extend_floor += step;
        let min_lifetime = *extend_floor;
        PlannedTx {
            label: "extend".into(),
            body: Body::Execute(vec![Operation::extend_expiry(key, 0, min_lifetime)]),
            gas: ENTITY_GAS,
            expect: Expect::Success,
            effects: vec![Effect::Extend { key, min_lifetime }],
        }
    }

    fn give_away(&mut self, live: &[B256]) -> PlannedTx {
        let key = *self.rng.pick(live);
        let effects = vec![Effect::Transfer { key, to: SINK }];
        self.commit(&effects);
        PlannedTx {
            label: "transfer".into(),
            body: Body::Execute(vec![Operation::transfer_ownership(key, SINK)]),
            gas: ENTITY_GAS,
            expect: Expect::Success,
            effects,
        }
    }

    fn delete(&mut self, live: &[B256]) -> PlannedTx {
        let key = *self.rng.pick(live);
        let effects = vec![Effect::Delete { key }];
        self.commit(&effects);
        PlannedTx {
            label: "delete".into(),
            body: Body::Execute(vec![Operation::delete(key)]),
            gas: ENTITY_GAS,
            expect: Expect::Success,
            effects,
        }
    }

    /// A batch that reverts: an op on an entity the sender deleted (not found),
    /// gave away (not its owner) or never had, sometimes after a valid create,
    /// which must then not land either.
    fn deliberate_revert(&mut self) -> PlannedTx {
        let dead: Vec<(B256, &'static str)> = self
            .entities
            .iter()
            .filter_map(|(key, e)| match e {
                Local::Deleted => Some((*key, "a deleted entity")),
                Local::Given => Some((*key, "an entity given away")),
                Local::Live { .. } => None,
            })
            .collect();
        let (key, target) = if dead.is_empty() || self.rng.chance(20) {
            let ghost = B256::from_slice(
                &(0..4)
                    .flat_map(|_| self.rng.next_u64().to_be_bytes())
                    .collect::<Vec<u8>>(),
            );
            (ghost, "an entity that never existed")
        } else {
            *self.rng.pick(&dead)
        };
        let (failing, failing_effect, op) = if self.rng.chance(50) {
            let value = self.value();
            (
                Operation::patch(key, vec![set_attr((&"note".to_string(), &value))]),
                Effect::Set {
                    key,
                    name: "note".into(),
                    value,
                },
                "patch",
            )
        } else {
            (Operation::delete(key), Effect::Delete { key }, "delete")
        };
        let mut ops = Vec::new();
        let mut effects = Vec::new();
        let mut label = format!("{op} of {target}");
        if self.rng.chance(50) {
            let (create, effect) = self.create_op(self.minted);
            ops.push(create);
            effects.push(effect);
            label = format!("create, then {label}");
        }
        ops.push(failing);
        effects.push(failing_effect);
        PlannedTx {
            label,
            body: Body::Execute(ops),
            gas: ENTITY_GAS,
            expect: Expect::Revert,
            effects,
        }
    }

    /// Fold a successful transaction's effects into the sender's view.
    fn commit(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::Create { key, attrs, .. } => {
                    self.entities.insert(
                        *key,
                        Local::Live {
                            attrs: attrs.clone(),
                            extend_floor: EXTEND_FLOOR,
                        },
                    );
                }
                Effect::Set { key, name, value } => {
                    if let Some(Local::Live { attrs, .. }) = self.entities.get_mut(key) {
                        attrs.insert(name.clone(), value.clone());
                    }
                }
                Effect::Unset { key, name } => {
                    if let Some(Local::Live { attrs, .. }) = self.entities.get_mut(key) {
                        attrs.remove(name);
                    }
                }
                Effect::Extend { .. } => {}
                Effect::Transfer { key, .. } => {
                    self.entities.insert(*key, Local::Given);
                }
                Effect::Delete { key } => {
                    self.entities.insert(*key, Local::Deleted);
                }
            }
        }
    }
}

/// A string attribute, indexed as its name always is.
fn set_attr((name, value): (&String, &String)) -> Attribute {
    let indexed = NAMES
        .iter()
        .find(|(n, _)| n == name)
        .is_some_and(|(_, indexed)| *indexed);
    Attribute::from_value(
        Ident32::encode(name).unwrap(),
        &AttributeValue::Str(value.clone()),
        indexed,
    )
    .unwrap()
}

/// The calldata of `execute(ops)`.
pub fn execute_calldata(ops: &[Operation]) -> Vec<u8> {
    IEntityRegistry::executeCall { ops: ops.to_vec() }.abi_encode()
}

/// What the pool requires at least: the transaction's terms, or the EIP-7623
/// calldata floor if higher.
fn intrinsic_gas(data: &[u8]) -> u64 {
    let zero = data.iter().filter(|b| **b == 0).count() as u64;
    let nonzero = data.len() as u64 - zero;
    let initial = 21_000 + 4 * zero + 16 * nonzero;
    let floor = 21_000 + 10 * (zero + 4 * nonzero);
    initial.max(floor)
}
