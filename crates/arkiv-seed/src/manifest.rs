//! What a seed run produced, for the tooling that consumes it: arkiv-prefill's
//! verify stage and watcher predict keys and counts from this rather than
//! re-deriving them.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// A description of one seeded genesis, written next to it.
///
/// The fields up to `owner_nonces` are the MPT seeder's, unchanged, so a
/// consumer of either reads both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedManifest {
    /// The genesis chain id; minted keys were derived under it.
    pub chain_id: u64,
    /// How many entities were seeded.
    pub count: u64,
    /// Every entity's payload length, for a synthetic seed; 0 for an input.
    pub payload_size: usize,
    /// Every entity's content type, for a synthetic seed; empty for an input.
    pub content_type: String,
    /// A synthetic seed's owners, in the order entities were dealt to them;
    /// an input's distinct creators.
    pub owners: Vec<Address>,
    /// Every entity's expiry block, for a synthetic seed.
    pub expires_at: u64,
    /// A synthetic seed's attribute templates, in `name:type=expr` form.
    pub attributes: Vec<String>,
    /// The payload PRNG seed.
    pub seed: u64,
    /// The store's root over the genesis commit: the genesis `stateHash`.
    pub state_root: B256,
    /// Records in the genesis commit besides entities: the account records.
    pub accounts: u64,
    /// The keys of the first entities, in seed order.
    pub sample_keys: Vec<B256>,
    /// Each owner's entity nonce after genesis -- the nonce its next create
    /// mints from. Zero for an explicit-key input.
    pub owner_nonces: BTreeMap<Address, u64>,
    /// The genesis hash the commit is tagged with.
    pub genesis_hash: B256,
    /// Where the genesis commit was written: the store namespace.
    pub store_namespace: String,
}
