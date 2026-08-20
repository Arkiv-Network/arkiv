//! The reth-host implementation of the Arkiv **state manager**.
//!
//! The stores live elsewhere — the consensus lanes in
//! `arkiv-reth-mpt-committed-store`, the node-local ones in
//! `arkiv-reth-uncommitted-store`. This crate holds what sits *around* them:
//!
//! - [`MptStateManager`] — the
//!   [`StateManager`](arkiv_interfaces::manager::StateManager) implementation:
//!   every store composed behind the one seam the business layer, the
//!   executor, and the read paths go through — including pricing
//!   (`get_operation_cost`).
//! - [`WriteOverlay`] — the write-path base: bridges revm's `Database` to the
//!   raw store seams and accumulates every lane's writes into the one
//!   `EvmState` diff reth commits. [`write_manager`] is the ready-made
//!   composition of the two.

pub mod manager;
pub mod overlay;

pub use manager::{MptError, MptStateManager};
pub use overlay::WriteOverlay;

use reth_ethereum::evm::primitives::Database;

/// The write-path state manager: every state lane multiplexed over one
/// [`WriteOverlay`] diff, so a transaction's entities, index writes, minting
/// nonce, and sender accounting land in a single `EvmState`
/// (`mgr.into_base().into_state()`).
pub type WriteManager<'a, DB> = MptStateManager<WriteOverlay<'a, DB>>;

/// A [`WriteManager`] over `db`.
pub fn write_manager<DB: Database>(db: &mut DB) -> WriteManager<'_, DB> {
    MptStateManager::new(WriteOverlay::new(db))
}
