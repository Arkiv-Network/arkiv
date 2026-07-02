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
//! reference. First in: [`layout`] — the entity address anchor.

pub mod layout;
