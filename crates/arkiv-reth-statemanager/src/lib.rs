//! The reth-host `StateView`: [`MptStateView`] composes the per-store logic
//! from the store crates behind one handle, and [`WriteOverlay`] bridges revm's
//! `Database` to the raw seams, accumulating committed writes into the one
//! `EvmState` diff reth commits. [`write_manager`] composes the two.

pub mod authenticated;
pub mod chain;
pub mod manager;
pub mod overlay;

pub use manager::{MptError, MptStateView};
pub use overlay::WriteOverlay;

use arkiv_interfaces::statemanager::BlockRef;
use reth_ethereum::evm::primitives::Database;

/// The write-path view: a transaction's every effect lands in a single
/// `EvmState` (`view.into_base().into_state()`) once committed.
pub type WriteManager<'a, DB> = MptStateView<WriteOverlay<'a, DB>>;

pub fn write_manager<DB: Database>(db: &mut DB, parent: BlockRef) -> WriteManager<'_, DB> {
    MptStateView::new(WriteOverlay::new(db), parent)
}

pub mod genesis;
