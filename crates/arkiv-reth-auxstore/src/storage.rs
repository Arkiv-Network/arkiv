//! The storage-slot seam the tier-2 index runs over.
//!
//! The tier-2 range index (the int-mode B+ tree, and later the string cascade)
//! lives in account **storage slots**, not code: an ordered structure the host
//! can't get from reth's unordered slot map for free, so we build it inside the
//! slots ourselves. [`IndexStorage`] is the minimal read/write interface that
//! construction needs — the analogue of the entity store's `AccountCode` code
//! seam, but for storage.
//!
//! Keeping it a trait means the B+ tree code is pure logic with no reth in sight:
//! the production impl bridges these three methods to revm/reth state (a later
//! module), while the tests drive them against an in-memory map.

use alloy_primitives::{Address, B256};

/// Read/write access to account storage slots, for building the tier-2 index.
///
/// Conventions, matching reth's storage model:
/// - [`storage`](IndexStorage::storage) returns [`B256::ZERO`] for a slot that was
///   never written (absent and explicitly-zero are indistinguishable — which is
///   why the index encodes "present" as `len + 1`, never `0`).
/// - [`ensure_account_persists`](IndexStorage::ensure_account_persists) raises an
///   account's nonce so EIP-161 doesn't prune it at end-of-block. An account that
///   only ever receives *storage* writes is otherwise still "empty" to EIP-161
///   (which ignores storage) and would be swept away with its slots. Idempotent.
pub trait IndexStorage {
    /// Error type — the host's choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// The value at `addr`'s storage `slot`, or [`B256::ZERO`] if never written.
    fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Self::Error>;

    /// Write `value` to `addr`'s storage `slot`.
    fn set_storage(&mut self, addr: Address, slot: B256, value: B256) -> Result<(), Self::Error>;

    /// Ensure `addr` survives end-of-block pruning (see the trait docs).
    fn ensure_account_persists(&mut self, addr: Address) -> Result<(), Self::Error>;
}

#[cfg(test)]
pub(crate) use mock::MemStorage;

#[cfg(test)]
mod mock {
    use super::*;
    use std::collections::HashMap;

    /// An in-memory [`IndexStorage`] for tests: a flat `(addr, slot) -> value`
    /// map, with the "absent reads as zero" convention.
    #[derive(Debug, Default)]
    pub struct MemStorage {
        slots: HashMap<(Address, B256), B256>,
    }

    impl IndexStorage for MemStorage {
        type Error = core::convert::Infallible;

        fn storage(&mut self, addr: Address, slot: B256) -> Result<B256, Self::Error> {
            Ok(self.slots.get(&(addr, slot)).copied().unwrap_or(B256::ZERO))
        }

        fn set_storage(
            &mut self,
            addr: Address,
            slot: B256,
            value: B256,
        ) -> Result<(), Self::Error> {
            self.slots.insert((addr, slot), value);
            Ok(())
        }

        fn ensure_account_persists(&mut self, _addr: Address) -> Result<(), Self::Error> {
            Ok(())
        }
    }
}
