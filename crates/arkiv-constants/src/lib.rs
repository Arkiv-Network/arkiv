//! Protocol-wide byte widths, named once.
//!
//! An address is 20 bytes and an EVM word is 32 — sizes the Arkiv encodings are
//! built on that aren't obvious from any primitive type. They turn up as raw
//! `[..20]` / `[0u8; 32]` slicing in address derivation, the record codec, and the
//! index's storage layout across several crates; naming them here gives those sites
//! one shared vocabulary. (Widths that *are* obvious from their type — a `u64` is
//! `size_of::<u64>()` bytes — stay inline; they don't earn a name.)
//!
//! Crate-*local* magic numbers (a specific struct's field offsets, a domain tag's
//! length, an ABI selector) stay in their own crate — only genuinely protocol-wide
//! widths live here.

#![no_std]
#![forbid(unsafe_code)]

/// Length of an address, in bytes.
///
/// Matches `arkiv_interfaces::primitives::Address` (`[u8; ADDRESS_LEN]`) and an
/// Ethereum account address.
pub const ADDRESS_LEN: usize = 20;

/// Length of a 256-bit EVM word, in bytes — also the width of a hash, a commitment
/// root, and an entity key.
///
/// Matches `arkiv_interfaces::primitives::Hash` (`[u8; WORD_LEN]`).
pub const WORD_LEN: usize = 32;

// Invariants, checked at compile time — a constants crate's guarantees belong here,
// not in runtime tests. An address fits inside a word (everything that right-aligns
// an address into a slot relies on it), and the widths match the arrays they name.
const _: () = assert!(ADDRESS_LEN < WORD_LEN);
const _: () = assert!(ADDRESS_LEN == size_of::<[u8; ADDRESS_LEN]>());
const _: () = assert!(WORD_LEN == size_of::<[u8; WORD_LEN]>());
