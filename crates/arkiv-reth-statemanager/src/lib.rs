//! The reth-host `StateView`.
//!
//! [`HostStateView`] is the one view the node executes against, and every lane
//! of it is a GolemDB store's: entities, the query index, the pruning set, the
//! minting nonces, and — since the accounts cutover — balances and transaction
//! nonces too.
//!
//! The two account lanes are additionally *mirrored* into the [`EvmState`] diff
//! reth commits, which is what [`WriteOverlay`] and the [`accounts`] seams are
//! for. Nothing reads that diff back; it exists so reth's per-block account
//! cache does not serve a sender stale between two of its own transactions.
//!
//! [`host_manager`] composes the two and opens the store branch an execution
//! runs on. [`GolemAccounts`] is the read side, for the state provider, and
//! [`seed_genesis`] puts the chain's allocation in the store to begin with.
//!
//! [`EvmState`]: reth_ethereum::evm::revm::state::EvmState

pub mod accounts;
pub mod genesis;
pub mod host;
pub mod overlay;
pub mod provider;

pub use accounts::{BalanceAccess, NonceAccess};
pub use genesis::{GenesisAccount, seed_genesis};
pub use host::{BlockSeals, HostError, HostStateView, HostStore, host_manager, open_block_branch};
pub use overlay::WriteOverlay;
pub use provider::{AccountReadError, GolemAccounts, StoredAccount};
