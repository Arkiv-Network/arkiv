//! Construct genesis through the live entity executor and authenticated state view.
//!
//! Native funding/predeploys and one root account form the Ethereum alloc. The
//! manifest carries a portable authenticated snapshot for `config.arkivState`.
//! `MemorySink` and `StreamSink` collect/export native accounts; custom snapshot
//! construction currently uses memory. See the authenticated-store README.

mod build;
pub mod export;
mod manifest;
mod sink;
mod sort;
mod spec;

pub use build::{Progress, SeededState, build, build_in_memory};
pub use manifest::SeedManifest;
pub use sink::{AccountSink, Finished, MemorySink, StreamSink};
pub use spec::{AttributeTemplate, SeedSpec, ValueTemplate, ValueType};

/// Entities per executor batch when a spec does not say: large enough that the
/// per-batch index rewrite is amortised, small enough that a batch's staged
/// overlay stays cheap to commit.
pub const DEFAULT_BATCH_SIZE: usize = 10_000;
