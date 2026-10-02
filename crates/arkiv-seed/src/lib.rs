//! Pre-populating an Arkiv chain: build the state a network should hold at
//! block 0 *before* the network exists.
//!
//! The problem this solves is scale. Filling a devnet through transactions
//! tops out at a few thousand entities per minute and produces a different
//! chain every run. Seeding instead constructs the **committed state** —
//! entity records, the query index, minting nonces — offline, and hands it to
//! reth as genesis state, so every node starts from the same, arbitrarily large
//! dataset at block 0.
//!
//! ## How it stays consensus-correct
//!
//! Nothing here encodes a storage layout by hand. A [`SeedSpec`] is turned into
//! ordinary `Op::Create`s and pushed through the very code the node runs per
//! block: the executor stages entities into a `MptStateView` over the
//! production `WriteOverlay`, the store commits them as account code, the index
//! folds them in — over revm's in-memory `CacheDB` instead of MDBX. The one
//! deviation is the index's bulk insert path
//! ([`RethAuxStore::apply_inserts_bulk`](arkiv_reth_mpt_committed_store::RethAuxStore::apply_inserts_bulk)),
//! which writes each pair bitmap once per batch rather than once per entity —
//! byte-identical pair accounts, and the only thing that keeps a million-entity
//! seed from being quadratic.
//!
//! ## Three ways out
//!
//! Accounts leave the builder as they are finished, into an [`AccountSink`]:
//! a [`MemorySink`] collects them for the two JSON shapes, a [`StreamSink`]
//! writes the dump and builds the root on the way, bounded by its sort
//! buffers rather than by the seed. The shapes:
//!
//! - a **genesis JSON** with the accounts in `alloc` — `arkiv-reth node --chain
//!   <file>`, or any tool that consumes a geth-format genesis. reth derives the
//!   genesis state root from the alloc, so this is the route for anything a
//!   node can parse and hash in memory (low millions of accounts);
//! - an **alloc-only JSON** — ethereum-package's
//!   `network_params.additional_preloaded_contracts`, which is how the Kurtosis
//!   devnet seeds both its sequencer and its follower from one genesis;
//! - a **JSONL state dump** plus a genesis carrying `stateHash` and an empty
//!   `alloc` — `arkiv-reth init-state --chain <genesis> <dump>` streams the
//!   accounts into MDBX through reth's ETL importer and recomputes the root on
//!   disk, which is the route past what fits in memory: the seeder streams it
//!   the same way, one account per line as each is finished, the root built
//!   through an external sort. The chain spec parser honours `stateHash`
//!   (geth's field for exactly this case).
//!
//! Entity keys are minted with the node's own derivation from each owner's
//! minting nonce, and the nonces are written to the system account, so a client
//! predicting the next key after genesis — the SDK's `entityNonce` flow — is
//! never surprised.

mod build;
pub mod export;
mod manifest;
mod sink;
mod sort;
mod spec;

pub use build::{Progress, SeededState, build, build_in_memory};
pub use manifest::SeedManifest;
pub use sink::{AccountSink, Finished, MemorySink, StreamSink};
pub use spec::{AttributeTemplate, SeedSpec, ValueTemplate, ValueType};

/// Entities per executor batch when a spec does not say: large enough that the
/// per-batch index rewrite is amortised, small enough that a batch's staged
/// overlay stays cheap to commit.
pub const DEFAULT_BATCH_SIZE: usize = 10_000;
