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
//! This is host-specific by design. Per the Arkiv/host boundary (see the
//! `arkiv-interfaces` crate docs), *where* an entity physically lives — which
//! account address, which storage slot — is reth's concern, not the Arkiv
//! specification's. The spec says "there is a committed entity store"; this crate
//! is one concrete answer to that on reth.
//!
//! It is being ported, module by module, from the proven `arkiv-db-engine`
//! reference. Here so far: [`layout`] — the entity address anchor; [`record`] —
//! the versioned `Entity` ⇄ account-code byte codec (`0xFE00 || RLP`), which
//! implements the spec's [`EntityCodec`](arkiv_interfaces::codec::EntityCodec) via
//! [`RecordCodec`](record::RecordCodec); [`store`] — the
//! [`EntityStore`](arkiv_interfaces::state::EntityStore) impl over an
//! [`EntityBackend`](store::EntityBackend) seam; [`account`] — the [`AccountCode`]
//! reth code seam; and [`backend`] — [`CodeBackend`], the `EntityBackend` that
//! keeps each entity in its account's code. The reth [`AccountCode`] bridges (over
//! a revm `Journal` and a `StateProvider` snapshot) land next.
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
