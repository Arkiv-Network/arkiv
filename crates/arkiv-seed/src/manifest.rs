//! What a seed run produced, for the tooling that consumes it: the harness
//! tests and the Kurtosis integration tests predict keys and counts from this
//! rather than re-deriving them.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// A description of one seeded state, written next to its output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedManifest {
    /// Portable custom records authenticated by the genesis root account.
    pub authenticated_state: arkiv_authenticated_store::Snapshot,
    /// The chain id the entity keys were derived under.
    pub chain_id: u64,
    /// How many entities were seeded.
    pub count: u64,
    /// Every entity's payload length in bytes.
    pub payload_size: usize,
    /// Every entity's content type.
    pub content_type: String,
    /// The owners, in the order entities were dealt to them (round-robin).
    pub owners: Vec<Address>,
    /// Every entity's expiry block.
    pub expires_at: u64,
    /// The attribute templates, in their `name:type=expr` spelling.
    pub attributes: Vec<String>,
    /// The payload PRNG seed.
    pub seed: u64,
    /// The state root of the whole alloc — what the genesis header carries.
    pub state_root: B256,
    /// Native account count: the root account, funding and predeploys.
    pub accounts: u64,
    /// The keys of the first entities, in seed order.
    pub sample_keys: Vec<B256>,
    /// Each owner's minting nonce after genesis — the nonce its next create
    /// mints from.
    pub owner_nonces: BTreeMap<Address, u64>,
}

impl SeedManifest {
    /// How many sample keys a manifest records.
    pub const SAMPLE_KEYS: usize = 16;
}
