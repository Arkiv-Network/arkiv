//! Arkiv's storage pricing, iteration 1: what a transaction's storage costs,
//! as pure, checked arithmetic.
//!
//! A sender pays for the storage units an entity takes, times the blocks it
//! keeps them. The executor measures; this crate prices:
//!
//! - [`units`]: storage units from a record's size (`#meta`), and the change a
//!   write made from its [`WriteDetails`](arkiv_interfaces::store::WriteDetails).
//! - [`storage_time`]: unit-blocks bought per operation, and the lifetime bound
//!   a create or extend must respect.
//! - [`GasMeter`]: one transaction's inputs, summed exactly, finished into a
//!   [`GasBreakdown`](arkiv_interfaces::gas::GasBreakdown) with the floor and
//!   the rounding applied once.
//! - [`rate`]: an entity's `$rate`, the wei per block its storage was bought at.
//!
//! Gas is `u64`, storage time and wei `u128`; nothing saturates. An
//! [`Overflow`] fails the transaction, identically on every node. Balances are
//! `U256` and stay with the executor, which widens these amounts losslessly.
#![no_std]

extern crate alloc;

pub mod error;
pub mod meter;
pub mod rate;
pub mod schedule;
pub mod schedule_record;
pub mod storage_time;
pub mod units;

pub use error::Overflow;
pub use meter::GasMeter;
pub use rate::RateUpdate;
pub use schedule::FeeSchedule;
pub use schedule_record::{FeeScheduleSource, ScheduleError};
