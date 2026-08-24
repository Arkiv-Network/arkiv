//! The entity lane: the reth-host
//! [`EntityStore`](arkiv_interfaces::state::EntityStore) — **the entities
//! themselves** — against reth's account/slot state model.
//!
//! Scope is the entity store *only*. The query index
//! ([`AuxiliaryStore`](arkiv_interfaces::state::AuxiliaryStore)) is a separate
//! lane and lives in [`indices`](crate::indices); this module holds no
//! index/auxiliary data.
//!
//! Host-specific by design: *where* an entity physically lives is reth's concern,
//! not the specification's. Ported from the `arkiv-db-engine` reference.
//!
//! [`layout`] — the entity address anchor; [`record`] — the versioned `Entity` ⇄
//! account-code byte codec (`0xFE00 || RLP`); [`store`] — the
//! [`EntityStore`](arkiv_interfaces::state::EntityStore) impl over an
//! [`EntityBackend`](store::EntityBackend) seam; [`account`] — the [`AccountCode`]
//! reth code seam; [`backend`] — [`CodeBackend`], the `EntityBackend` keeping each
//! entity in its account's code.
//!
//! [`AccountCode`]: account::AccountCode
//! [`CodeBackend`]: backend::CodeBackend

pub mod account;
pub mod backend;
pub mod layout;
pub mod record;
pub mod store;

pub use account::AccountCode;
pub use backend::{CodeBackend, CodeBackendError};
pub use record::{RecordError, decode, encode};
pub use store::{EntityBackend, RethEntityStore};
