//! [`AccountCode`] — the minimal reth state surface the entity store needs: read
//! and write an account's `code` at an address.
//!
//! An entity lives entirely in its account's `code` (`0xFE || RLP`, see
//! [`record`](crate::entities::record)), so this code lane is all [`CodeBackend`] needs. The
//! query index (storage slots) is a separate concern (`AuxiliaryStore`), not here.
//!
//! Keeping it a trait lets the entity-persistence logic ([`CodeBackend`]) be tested
//! without reth. The reth bridges implement it two ways — over a revm `Journal` for
//! the write path, and over a `StateProvider` snapshot for reads.
//!
//! [`CodeBackend`]: crate::entities::backend::CodeBackend

use alloy_primitives::Address;

/// Read and write an account's `code`.
pub trait AccountCode {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The account's code, or an empty vec if the account is absent or has none.
    fn code(&mut self, addr: Address) -> Result<Vec<u8>, Self::Error>;

    /// Set the account's code, creating and persisting the account.
    fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Self::Error>;

    /// Clear the account's code (tombstone), keeping the account alive so it isn't
    /// pruned.
    fn clear_code(&mut self, addr: Address) -> Result<(), Self::Error>;
}

/// Forwarding impl, so a seam consumer (a [`CodeBackend`](crate::entities::backend::CodeBackend),
/// which takes its backend by value) can be built over a *borrowed* backend too —
/// e.g. a state manager lending out its one underlying state for the duration of
/// a call.
impl<T: AccountCode + ?Sized> AccountCode for &mut T {
    type Error = T::Error;

    fn code(&mut self, addr: Address) -> Result<Vec<u8>, Self::Error> {
        (**self).code(addr)
    }

    fn set_code(&mut self, addr: Address, code: Vec<u8>) -> Result<(), Self::Error> {
        (**self).set_code(addr, code)
    }

    fn clear_code(&mut self, addr: Address) -> Result<(), Self::Error> {
        (**self).clear_code(addr)
    }
}
