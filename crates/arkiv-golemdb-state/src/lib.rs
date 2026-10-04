//! Arkiv's committed stores over a GolemDB `Store`, with entities as native records.
//!
//! Replaces `arkiv-reth-mpt-committed-store`. Most of that crate is index machinery
//! built because Ethereum's account trie has none; a GolemDB store indexes attribute
//! cells itself, so [`indices`] is a lowering into store queries and nothing more.
#![no_std]

extern crate alloc;

pub mod accounts;
pub mod entities;
pub mod indices;
pub mod view;

pub use view::{GolemStateView, ViewError};
