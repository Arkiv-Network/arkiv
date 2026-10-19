//! Prefilling an Arkiv chain on GolemDB: the entities a network should hold at
//! block 0, written into the Arkiv Store's genesis commit before the network
//! exists.
//!
//! # Why
//!
//! Filling a devnet through transactions tops out at a few thousand entities
//! a minute and produces a different chain every run. A prefill instead writes
//! the **committed state** directly, so every node starts from the same,
//! arbitrarily large dataset at block 0.
//!
//! # How it stays consensus-correct
//!
//! On GolemDB an entity is one store record, and the store indexes its
//! attribute cells itself -- there is no index state to construct. So a seed
//! entity is mapped with [`entity_records::to_cells`], the same spec-level
//! mapping the executor's writes go through, and written with an ordinary
//! `create` into one branch. The accounts go in through the node's own
//! [`write_genesis_accounts`]; a creator whose keys were minted gets the
//! `$minted` nonce the executor would have advanced to.
//!
//! The branch is then **sealed**, and the store's root becomes the genesis
//! header's `stateHash`. Only after that is the commit made, tagged with the
//! resulting genesis hash -- which is how the node finds and checks it.
//!
//! The root is whatever the store computes, so a seed must be written into the
//! same kind of store the network runs on: two backends that hash differently
//! give two different genesis hashes for the same seed.
//!
//! # Two sources
//!
//! - [`SeedSpec`]: a synthetic dataset, every entity a pure function of its
//!   ordinal, keys minted the way the node mints them.
//! - [`Input`]: explicit-key JSON Lines, as the Sourcify export writes them
//!   (`seed.json` plus entity parts).
//!
//! [`entity_records::to_cells`]: arkiv_interfaces::entity_records::to_cells
//! [`write_genesis_accounts`]: arkiv_reth_statemanager::write_genesis_accounts

pub mod genesis;
mod input;
mod manifest;
mod spec;
mod writer;

pub use input::{Input, count_entities};
pub use manifest::SeedManifest;
pub use spec::{AttributeTemplate, SeedSpec, ValueTemplate, ValueType};
pub use writer::{GenesisWriter, SAMPLE_KEYS, SealedGenesis, Summary};

use arkiv_interfaces::entity::Entity;

/// One entity of a seed, as it will stand at block 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEntity {
    pub entity: Entity,
    /// Whether the key was minted from the creator's entity nonce, which the
    /// genesis then has to have advanced. Explicit keys mint nothing.
    pub minted: bool,
}
