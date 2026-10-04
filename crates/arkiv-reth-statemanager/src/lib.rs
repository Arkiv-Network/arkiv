//! The reth-host `StateView`.
//!
//! [`HostStateView`] is the one view the node executes against: Arkiv's own
//! state — entities, the query index, the pruning set, the minting nonces —
//! in a GolemDB store, and the two lanes Ethereum also reads (an account's
//! balance and nonce) staged into the [`EvmState`] diff reth commits.
//!
//! [`WriteOverlay`] is that second half: it bridges revm's `Database` to the
//! [`accounts`] seams and accumulates the diff. [`host_manager`] composes the
//! two and opens the store branch an execution runs on.
//!
//! [`EvmState`]: reth_ethereum::evm::revm::state::EvmState

pub mod accounts;
pub mod host;
pub mod overlay;

pub use accounts::{BalanceAccess, NonceAccess};
pub use host::{BlockSeals, HostError, HostStateView, HostStore, host_manager, open_block_branch};
pub use overlay::WriteOverlay;
