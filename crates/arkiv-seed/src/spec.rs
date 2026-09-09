//! What to seed: the shape of the synthetic dataset, and how entity `i` is
//! derived from it.
//!
//! Everything is a pure function of the spec and the entity's ordinal, so two
//! runs of one spec build the same state — and the same state root.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use alloy_primitives::Address;
use arkiv_bindings::Ident32;
use arkiv_interfaces::constants::{ENTITY_MAX_ATTRIBUTES, MAX_PAYLOAD_BYTES, MAX_STR_BYTES};
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags};
use arkiv_interfaces::execution::Op;
use arkiv_interfaces::primitives::EntityCreationNonce;
use arkiv_reth_executor::derive_entity_address;
use eyre::{Result, bail, ensure};

use crate::DEFAULT_BATCH_SIZE;

/// The dataset to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedSpec {
    /// The chain id entity keys are derived under — must be the genesis chain
    /// id, or the SDK's key prediction disagrees with the node after genesis.
    pub chain_id: u64,
    /// How many entities to create.
    pub count: u64,
    /// Every entity's payload length.
    pub payload_size: usize,
    /// Every entity's content type.
    pub content_type: String,
    /// The creators/owners, dealt round-robin. Each owner's minting nonce
    /// advances once per entity it receives.
    pub owners: Vec<Address>,
    /// Every entity's absolute expiry block; `u64::MAX` never expires.
    pub expires_at: u64,
    /// User attributes, derived per entity from the templates.
    pub attributes: Vec<AttributeTemplate>,
    /// Seeds the payload byte generator.
    pub seed: u64,
    /// Entities per executor batch.
    pub batch_size: usize,
}

impl SeedSpec {
    /// A spec with the defaults: one thousand 1 KiB entities, one owner, a
    /// `rank` uint bucketed mod 100 and a `team` string cycling three names.
    pub fn new(chain_id: u64, owners: Vec<Address>) -> Self {
        Self {
            chain_id,
            count: 1000,
            payload_size: 1024,
            content_type: "application/octet-stream".to_string(),
            owners,
            expires_at: u64::MAX,
            attributes: Self::default_attributes(),
            seed: 0,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// The attribute templates a spec carries unless told otherwise.
    pub fn default_attributes() -> Vec<AttributeTemplate> {
        vec![
            "rank:u256=mod(100)".parse().expect("valid template"),
            "team:str=cycle(red,green,blue)"
                .parse()
                .expect("valid template"),
        ]
    }

    /// Reject a spec the executor or the store would refuse later.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.owners.is_empty(), "a seed needs at least one owner");
        ensure!(
            self.owners.iter().all(|o| *o != Address::ZERO),
            "the zero address cannot own entities"
        );
        ensure!(
            self.payload_size <= MAX_PAYLOAD_BYTES,
            "payload size {} exceeds the protocol maximum of {MAX_PAYLOAD_BYTES} bytes",
            self.payload_size
        );
        ensure!(
            self.expires_at > 0,
            "expires_at must be a future block (an entity expires at block 0 before it exists)"
        );
        ensure!(self.batch_size > 0, "batch size must be positive");
        ensure!(
            self.attributes.len() <= ENTITY_MAX_ATTRIBUTES,
            "{} attributes exceed the entity maximum of {ENTITY_MAX_ATTRIBUTES}",
            self.attributes.len()
        );
        let mut names = BTreeMap::new();
        for template in &self.attributes {
            if names.insert(template.name.clone(), ()).is_some() {
                bail!("duplicate attribute name {:?}", template.name);
            }
        }
        Ok(())
    }

    /// The owner of entity `i`.
    pub fn owner_of(&self, i: u64) -> Address {
        self.owners[(i % self.owners.len() as u64) as usize]
    }

    /// The minting nonce entity `i` uses: how many entities its owner received
    /// before it.
    pub fn nonce_of(&self, i: u64) -> EntityCreationNonce {
        EntityCreationNonce::new(i / self.owners.len() as u64)
    }

    /// Each owner's minting nonce once every entity is created.
    pub fn owner_nonces(&self) -> BTreeMap<Address, u64> {
        let owners = self.owners.len() as u64;
        self.owners
            .iter()
            .enumerate()
            .map(|(slot, owner)| {
                let slot = slot as u64;
                let dealt = if self.count > slot {
                    (self.count - slot).div_ceil(owners)
                } else {
                    0
                };
                (*owner, dealt)
            })
            .collect()
    }

    /// The `Create` for entity `i`, keyed exactly as the node would key the
    /// owner's `nonce_of(i)`-th create.
    pub fn create_op(&self, i: u64) -> Result<Op> {
        let owner = self.owner_of(i);
        let key = derive_entity_address(self.chain_id, &owner.into_array(), self.nonce_of(i), 0);
        let mut attributes = Vec::with_capacity(self.attributes.len());
        for template in &self.attributes {
            attributes.push(Attribute::new(
                template.name.as_bytes().to_vec(),
                template.value_for(i)?,
            ));
        }
        // The stored order is canonical: strictly ascending by name, which the
        // node's decoder enforces on the wire.
        attributes.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(Op::Create {
            key,
            expires_at: self.expires_at,
            creation_flags: CreationFlags::NONE,
            content_type: self.content_type.clone().into_bytes(),
            payload: self.payload_for(i),
            attributes,
        })
    }

    /// Entity `i`'s payload: a readable header naming the entity, then
    /// pseudo-random filler up to `payload_size`. Random so it neither
    /// compresses away in transport nor repeats across entities.
    pub fn payload_for(&self, i: u64) -> Vec<u8> {
        let mut payload = format!("arkiv-seed entity {i}\n").into_bytes();
        payload.truncate(self.payload_size);
        let mut rng = SplitMix64::new(self.seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        while payload.len() < self.payload_size {
            let word = rng.next().to_le_bytes();
            let take = (self.payload_size - payload.len()).min(word.len());
            payload.extend_from_slice(&word[..take]);
        }
        payload
    }
}

/// A user attribute derived per entity: `name:type=expr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeTemplate {
    pub name: String,
    pub ty: ValueType,
    pub value: ValueTemplate,
}

/// The attribute types a template can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    U256,
    U64,
    I32,
    Bool,
    Str,
}

impl ValueType {
    fn name(self) -> &'static str {
        match self {
            Self::U256 => "u256",
            Self::U64 => "u64",
            Self::I32 => "i32",
            Self::Bool => "bool",
            Self::Str => "str",
        }
    }
}

/// How a template's value depends on the entity ordinal `i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueTemplate {
    /// `i` itself — a distinct value per entity, the index's worst case.
    Index,
    /// `i mod n` — `n` distinct values, evenly spread.
    Mod(u64),
    /// The same literal on every entity.
    Const(String),
    /// The `i`-th of a fixed list, wrapping.
    Cycle(Vec<String>),
    /// A string prefix followed by `i` — distinct, prefix-queryable strings.
    Prefix(String),
}

impl AttributeTemplate {
    /// The value for entity `i`, typed as the template says.
    pub fn value_for(&self, i: u64) -> Result<AttributeValue> {
        let literal = |text: &str| parse_literal(self.ty, text);
        match &self.value {
            ValueTemplate::Index => Ok(from_ordinal(self.ty, i)),
            ValueTemplate::Mod(n) => Ok(from_ordinal(self.ty, i % n)),
            ValueTemplate::Const(text) => literal(text),
            ValueTemplate::Cycle(values) => literal(&values[(i % values.len() as u64) as usize]),
            ValueTemplate::Prefix(prefix) => {
                ensure!(
                    self.ty == ValueType::Str,
                    "prefix(...) only produces str attributes"
                );
                let text = format!("{prefix}{i}");
                ensure!(
                    text.len() <= MAX_STR_BYTES,
                    "prefixed value {text:?} exceeds {MAX_STR_BYTES} bytes"
                );
                Ok(AttributeValue::Str(text))
            }
        }
    }
}

/// An ordinal as a value of type `ty`: numeric types take it verbatim, `bool`
/// its parity, `str` its decimal spelling.
fn from_ordinal(ty: ValueType, n: u64) -> AttributeValue {
    match ty {
        ValueType::U256 => AttributeValue::u256_from_u64(n),
        ValueType::U64 => AttributeValue::U64(n),
        ValueType::I32 => AttributeValue::Int((n % (i32::MAX as u64 + 1)) as i32),
        ValueType::Bool => AttributeValue::Bool(n % 2 == 1),
        ValueType::Str => AttributeValue::Str(n.to_string()),
    }
}

fn parse_literal(ty: ValueType, text: &str) -> Result<AttributeValue> {
    Ok(match ty {
        ValueType::U256 => AttributeValue::u256_from_u64(text.parse()?),
        ValueType::U64 => AttributeValue::U64(text.parse()?),
        ValueType::I32 => AttributeValue::Int(text.parse()?),
        ValueType::Bool => AttributeValue::Bool(text.parse()?),
        ValueType::Str => {
            ensure!(
                text.len() <= MAX_STR_BYTES,
                "str value {text:?} exceeds {MAX_STR_BYTES} bytes"
            );
            AttributeValue::Str(text.to_string())
        }
    })
}

impl FromStr for AttributeTemplate {
    type Err = eyre::Report;

    /// `name:type=expr`, where `type` is one of `u256`, `u64`, `i32`, `bool`,
    /// `str` and `expr` one of `index`, `mod(n)`, `const(v)`, `cycle(a,b,…)`,
    /// `prefix(p)`.
    fn from_str(s: &str) -> Result<Self> {
        let (head, expr) = s
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("attribute template {s:?}: expected name:type=expr"))?;
        let (name, ty) = head
            .split_once(':')
            .ok_or_else(|| eyre::eyre!("attribute template {s:?}: expected name:type=expr"))?;
        let name = name.trim();
        // The node's decoder validates names as `Ident32`s; refusing an invalid
        // one here keeps the seed from writing what a client could never write.
        Ident32::encode(name).map_err(|e| eyre::eyre!("attribute name {name:?}: {e}"))?;
        let ty = match ty.trim() {
            "u256" => ValueType::U256,
            "u64" => ValueType::U64,
            "i32" => ValueType::I32,
            "bool" => ValueType::Bool,
            "str" | "string" => ValueType::Str,
            other => bail!("attribute template {s:?}: unknown type {other:?}"),
        };
        let expr = expr.trim();
        let call = |name: &str| -> Option<&str> {
            expr.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('('))
                .and_then(|rest| rest.strip_suffix(')'))
        };
        let value = if expr == "index" {
            ValueTemplate::Index
        } else if let Some(n) = call("mod") {
            let n: u64 = n.trim().parse()?;
            ensure!(n > 0, "attribute template {s:?}: mod(0) is undefined");
            ValueTemplate::Mod(n)
        } else if let Some(v) = call("const") {
            ValueTemplate::Const(v.trim().to_string())
        } else if let Some(list) = call("cycle") {
            let values: Vec<String> = list.split(',').map(|v| v.trim().to_string()).collect();
            ensure!(
                !values.is_empty() && values.iter().all(|v| !v.is_empty()),
                "attribute template {s:?}: cycle needs at least one non-empty value"
            );
            ValueTemplate::Cycle(values)
        } else if let Some(p) = call("prefix") {
            ValueTemplate::Prefix(p.trim().to_string())
        } else {
            bail!(
                "attribute template {s:?}: unknown expression {expr:?} (index, mod(n), const(v), cycle(a,b), prefix(p))"
            );
        };
        let template = Self {
            name: name.to_string(),
            ty,
            value,
        };
        // Typed literals must parse; find out now rather than at entity 0.
        template.value_for(0)?;
        Ok(template)
    }
}

impl fmt::Display for AttributeTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}=", self.name, self.ty.name())?;
        match &self.value {
            ValueTemplate::Index => write!(f, "index"),
            ValueTemplate::Mod(n) => write!(f, "mod({n})"),
            ValueTemplate::Const(v) => write!(f, "const({v})"),
            ValueTemplate::Cycle(values) => write!(f, "cycle({})", values.join(",")),
            ValueTemplate::Prefix(p) => write!(f, "prefix({p})"),
        }
    }
}

/// A tiny, portable PRNG (Vigna's SplitMix64): payload filler that is the same
/// on every machine without pulling in a randomness crate.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(n: u8) -> Vec<Address> {
        (1..=n).map(|b| Address::repeat_byte(b)).collect()
    }

    #[test]
    fn templates_parse_and_display_round_trip() {
        for text in [
            "rank:u256=mod(100)",
            "team:str=cycle(red,green,blue)",
            "id:u64=index",
            "flag:bool=mod(2)",
            "name:str=prefix(seed-)",
            "level:i32=const(-7)",
        ] {
            let template: AttributeTemplate = text.parse().unwrap();
            assert_eq!(template.to_string(), text);
        }
    }

    #[test]
    fn templates_reject_bad_input() {
        for text in [
            "rank=mod(3)",
            "Rank:u256=mod(3)",
            "rank:u256=mod(0)",
            "rank:u256=const(x)",
            "rank:u256=prefix(a)",
            "rank:float=index",
            "rank:u256=wat",
            "n:str=cycle()",
        ] {
            assert!(text.parse::<AttributeTemplate>().is_err(), "{text}");
        }
    }

    #[test]
    fn values_follow_the_ordinal() {
        let rank: AttributeTemplate = "rank:u256=mod(3)".parse().unwrap();
        assert_eq!(rank.value_for(4).unwrap(), AttributeValue::u256_from_u64(1));
        let team: AttributeTemplate = "team:str=cycle(a,b)".parse().unwrap();
        assert_eq!(team.value_for(3).unwrap(), AttributeValue::Str("b".into()));
        let flag: AttributeTemplate = "flag:bool=index".parse().unwrap();
        assert_eq!(flag.value_for(3).unwrap(), AttributeValue::Bool(true));
        let name: AttributeTemplate = "name:str=prefix(e-)".parse().unwrap();
        assert_eq!(
            name.value_for(12).unwrap(),
            AttributeValue::Str("e-12".into())
        );
    }

    #[test]
    fn owners_are_dealt_round_robin_with_advancing_nonces() {
        let mut spec = SeedSpec::new(1, owners(3));
        spec.count = 7;
        assert_eq!(spec.owner_of(0), spec.owner_of(3));
        assert_eq!(spec.nonce_of(0), EntityCreationNonce::new(0));
        assert_eq!(spec.nonce_of(3), EntityCreationNonce::new(1));
        assert_eq!(spec.nonce_of(4), EntityCreationNonce::new(1));
        let nonces = spec.owner_nonces();
        assert_eq!(nonces[&spec.owners[0]], 3);
        assert_eq!(nonces[&spec.owners[1]], 2);
        assert_eq!(nonces[&spec.owners[2]], 2);
    }

    #[test]
    fn keys_match_the_node_derivation() {
        let spec = SeedSpec::new(1337, owners(2));
        let Op::Create { key, .. } = spec.create_op(5).unwrap() else {
            panic!("create");
        };
        assert_eq!(
            key,
            derive_entity_address(
                1337,
                &spec.owners[1].into_array(),
                EntityCreationNonce::new(2),
                0
            )
        );
    }

    #[test]
    fn payloads_are_sized_deterministic_and_distinct() {
        let mut spec = SeedSpec::new(1, owners(1));
        spec.payload_size = 300;
        let a = spec.payload_for(1);
        assert_eq!(a.len(), 300);
        assert!(a.starts_with(b"arkiv-seed entity 1\n"));
        assert_eq!(a, spec.payload_for(1));
        assert_ne!(a, spec.payload_for(2));
        spec.payload_size = 5;
        assert_eq!(spec.payload_for(1), b"arkiv");
        spec.payload_size = 0;
        assert!(spec.payload_for(1).is_empty());
    }

    #[test]
    fn attributes_come_out_sorted() {
        let mut spec = SeedSpec::new(1, owners(1));
        spec.attributes = vec![
            "zeta:u64=index".parse().unwrap(),
            "alpha:u64=index".parse().unwrap(),
        ];
        let Op::Create { attributes, .. } = spec.create_op(0).unwrap() else {
            panic!("create");
        };
        assert_eq!(attributes[0].key, b"alpha");
        assert_eq!(attributes[1].key, b"zeta");
    }

    #[test]
    fn validation_catches_the_obvious() {
        let mut spec = SeedSpec::new(1, owners(1));
        spec.validate().unwrap();
        spec.expires_at = 0;
        assert!(spec.validate().is_err());
        let mut spec = SeedSpec::new(1, vec![]);
        assert!(spec.validate().is_err());
        spec.owners = owners(1);
        spec.attributes.push("rank:u64=index".parse().unwrap());
        assert!(spec.validate().is_err(), "duplicate name");
    }
}
