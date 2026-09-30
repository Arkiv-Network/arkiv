//! reth's key/value seam, implemented over a GolemDB [`Store`].
//!
//! [`Store`]: arkiv_interfaces::store::Store
//!
//! Implementing `reth_db_api::Database` rather than a provider is what lets every
//! reth table land in GolemDB while everything above the seam — the providers,
//! persistence, sync, pruning, reorg handling — stays reth's, unmodified. One generic
//! impl covers all 31 tables, because the seam deals in encoded bytes and never learns
//! what a header is.
//!
//! [`layout`] is the mapping from a table row to a record, and the part to review.
//!
//! # Two constraints the design works around
//!
//! **Cursors need range predicates, and only four cell types answer them.** `STR` is
//! prefix-only, so keys are ordered as a padded `U256`. See [`layout`].
//!
//! **`Store::query` reads commits, never a branch's uncommitted writes.** reth reads
//! its own writes inside a transaction, so a write transaction keeps an ordered
//! in-memory overlay of pending rows and cursors merge it with the committed scan.

pub mod layout;
