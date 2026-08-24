//! The reth-host implementation of Arkiv's **committed state** — every
//! consensus store, one module per lane, all multiplexed onto Ethereum's one
//! keccak-MPT account state.
//!
//! This crate is the physical side of the
//! [`CommittedState`](arkiv_interfaces::manager) picture: for each store the
//! `arkiv_interfaces` traits name, one module holds the reth layout that
//! realizes it —
//!
//! | lane                  | store                             | module                     | where it lives                     |
//! |-----------------------|-----------------------------------|----------------------------|------------------------------------|
//! | the entities          | [`RethEntityStore`]               | [`entities`]               | per-entity account code            |
//! | the query index       | [`RethAuxStore`]                  | [`indices`]                | bitmap code + storage-slot B-trees |
//! | account balances      | [`RethAccountBalancesStore`]      | [`account_balances`]       | the accounts themselves            |
//! | transaction nonces    | [`RethAccountNoncesStore`]        | [`account_nonces`]         | the accounts themselves            |
//! | entity-minting nonces | [`RethEntityCreationNoncesStore`] | [`entity_creation_nonces`] | system-account storage slots       |
//!
//! Each store is written against a **raw seam it owns**, not against reth: the
//! entity store's [`AccountCode`] (an account's code), the index's
//! [`IndexStorage`] (its storage slots), the balance store's [`BalanceAccess`]
//! and the nonce store's [`NonceAccess`] (the account's two remaining fields).
//! Together the seams cover everything of an Ethereum account Arkiv touches,
//! so one backend implementing all of them — the write overlay, a provider
//! snapshot, an in-memory mock — can carry every lane at once. That is what
//! `arkiv-reth-statemanager`'s `MptStateManager` does: compose these stores
//! over one base into the single `StateManager` seam.
//!
//! The **uncommitted** state (the pruning map) is deliberately absent: it
//! carries no commitment, so it lives with the manager, not here.
//!
//! Consensus warning: the address derivations ([`entities::layout`],
//! [`indices::address`]) and every stored byte format in this crate feed the
//! state root. They are locked by golden tests and must never drift silently.
//!
//! [`AccountCode`]: entities::AccountCode
//! [`IndexStorage`]: indices::IndexStorage
//! [`BalanceAccess`]: account_balances::BalanceAccess
//! [`NonceAccess`]: account_nonces::NonceAccess

pub mod account_balances;
pub mod account_nonces;
pub mod entities;
pub mod entity_creation_nonces;
pub mod indices;

pub use account_balances::{BalanceAccess, RethAccountBalancesStore};
pub use account_nonces::{NonceAccess, RethAccountNoncesStore};
pub use entities::{
    AccountCode, CodeBackend, CodeBackendError, EntityBackend, RecordError, RethEntityStore,
    decode, encode,
};
pub use entity_creation_nonces::RethEntityCreationNoncesStore;
pub use indices::{
    AuxError, Bitmap, BitmapError, Bound, IndexStorage, QueryCapabilities, RethAuxStore,
    all_entities_bucket, pair_address,
};
