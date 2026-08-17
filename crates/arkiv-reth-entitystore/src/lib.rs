//! The reth-host implementation of the Arkiv entity store.
//!
//! Sibling to `arkiv-reth-executor`. Where the executor implements the
//! [`arkiv_interfaces`] executor traits, this crate implements
//! [`EntityStore`](arkiv_interfaces::state::EntityStore) — **the entities
//! themselves** — against reth's account/slot state model.
//!
//! Scope is the entity store *only*. The query index
//! ([`AuxiliaryStore`](arkiv_interfaces::state::AuxiliaryStore)) is a separate
//! concern and lives in its own crate; this crate holds no index/auxiliary data.
//!
//! Host-specific by design: *where* an entity physically lives is reth's concern,
//! not the specification's. Ported from the `arkiv-db-engine` reference.
//!
//! [`layout`] — the entity address anchor; [`record`] — the versioned `Entity` ⇄
//! account-code byte codec (`0xFE00 || RLP`), implementing the spec's
//! [`EntityCodec`](arkiv_interfaces::codec::EntityCodec) via
//! [`RecordCodec`](record::RecordCodec); [`store`] — the
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
pub use record::{RecordCodec, RecordError, decode, encode};
pub use store::{EntityBackend, RethEntityStore};
