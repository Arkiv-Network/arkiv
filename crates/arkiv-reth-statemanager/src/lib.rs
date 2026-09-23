//! The reth-host `StateView`: [`MptStateView`] composes the Ethereum-side
//! stores (balances, transaction nonces) with the Arkiv database (entities,
//! minting nonces, the index) behind one handle. [`WriteOverlay`] bridges
//! revm's `Database` to the raw seams and accumulates the committed Ethereum
//! writes into the one `EvmState` diff reth commits; the database writes go
//! into a node staging set the caller flushes. [`write_manager`] composes
//! the two.

pub mod manager;
pub mod overlay;
pub mod seams;
pub mod testing;

pub use manager::{MptError, MptStateView};
pub use overlay::WriteOverlay;
pub use seams::{AnchorAccess, BalanceAccess, NonceAccess};

use arkiv_interfaces::statemanager::BlockRef;
use arkiv_store::{NodeReader, Staging};
use reth_ethereum::evm::primitives::Database;

/// The write-path view: a transaction's Ethereum effects land in a single
/// `EvmState` (`view.into_parts().0.into_state()`) once committed, and its
/// database effects in the staging set beside it.
pub type WriteManager<'a, DB, N> = MptStateView<WriteOverlay<'a, DB>, Staging<&'a N>>;

/// The error of a [`WriteManager`] over `N`.
pub type WriteError<N> = MptError<eyre::Report, <N as NodeReader>::Error>;

pub fn write_manager<'a, DB: Database, N: NodeReader>(
    db: &'a mut DB,
    nodes: &'a N,
    parent: BlockRef,
) -> Result<WriteManager<'a, DB, N>, WriteError<N>> {
    MptStateView::new(WriteOverlay::new(db), Staging::new(nodes), parent)
}
