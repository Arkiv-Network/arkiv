//! Protocol constants, named once.
//!
//! Two kinds of number live here, and only these two:
//!
//! - **Byte widths.** Sizes the Arkiv encodings are built on that aren't obvious
//!   from any primitive type. They turn up as raw `[..20]` / `[0u8; 32]` slicing
//!   in address derivation, the record codec, and the index's storage layout
//!   across several crates.
//! - **Protocol limits and addresses.** Numbers a *client* and the *engine* must
//!   agree on: how long a string may be, how many attributes an operation may
//!   carry, where entity calldata is sent. Each of these previously had two or
//!   three independent definitions across the ABI, query and host crates, which is
//!   the failure mode this module exists to prevent — a limit the SDK enforces at
//!   128 and the engine enforces at 129 is a consensus split, not a typo.
//!
//! Crate-*local* magic numbers stay in their own crate: a specific struct's field
//! offsets, a domain tag's length, an ABI selector, a node's page-size policy, the
//! index's B-tree order. The test is whether two crates have to agree on the value.
//! Widths that *are* obvious from their type — a `u64` is `size_of::<u64>()` bytes
//! — also stay inline; they don't earn a name.

// ── Byte widths ───────────────────────────────────────────────────────

/// Length of a 256-bit EVM word, in bytes — also the width of a hash, a commitment
/// root, an entity key, and **an Arkiv address**.
///
/// Matches [`Hash`](crate::primitives::Hash) (`[u8; WORD_LEN]`).
pub const WORD_LEN: usize = 32;

/// Width of an **Ethereum account address**, in bytes.
///
/// This is a property of the *host binding*, not of Arkiv. Arkiv's own address
/// space is [`WORD_LEN`] wide — an entity key is 32 bytes, and so is every
/// location the engine derives. 20 shows up only where a value really is an
/// Ethereum account, or where a 32-byte Arkiv value is squeezed into reth's
/// 20-byte account key.
///
/// Those two cases are genuinely different, and conflating them under one name is
/// what this constant's name is meant to stop:
///
/// **Really an Ethereum address** — 20 is correct and fixed by Ethereum:
/// - an entity's `owner` and `creator`, and the `addr` attribute type (EOAs that
///   sign transactions; EIP-55 checksums are defined over 20 bytes)
/// - [`ARKIV_ADDRESS`], a `CALL` target
/// - the ABI `address` type, right-aligned in a word by the encoder
///
/// **A 32-byte Arkiv value truncated to fit** — 20 is a storage artifact, and the
/// truncation costs 96 bits of the space Arkiv actually addresses in:
/// - `arkiv-reth-auxstore`'s derived index accounts, every one of them
///   `keccak256(…)[..ETH_ADDRESS_LEN]`. These hold no keys and no signer; they are
///   storage buckets that must wear an address because reth keys accounts by one.
/// - `arkiv-reth-entitystore`'s `entity_address`, the first 20 bytes of a 32-byte
///   entity key.
///
/// The second group is what a host-agnostic Arkiv would not do. It is deliberate
/// for now — reth's account model gives no wider key — but it is a host
/// adaptation, so it is named as one rather than passed off as a protocol width.
pub const ETH_ADDRESS_LEN: usize = 20;

// ── The Arkiv address ─────────────────────────────────────────────────

/// The Arkiv address — `0x4400…0044`.
///
/// EOAs and SDKs `CALL` here with `execute(Operation[])` / `nonces(address)`
/// calldata. There is no precompile object and no bytecode: the executor routes
/// calls to this address directly, and registration is programmatic, so genesis
/// carries no allocation for it.
///
/// Genuinely an Ethereum address — a transaction names it as its `to` — so
/// [`ETH_ADDRESS_LEN`] is the right width here, not a truncation.
///
/// Raw bytes rather than an alloy `Address` because this crate names no external
/// types; hosts wrap it (`Address::new(ARKIV_ADDRESS)`) at their boundary.
pub const ARKIV_ADDRESS: [u8; ETH_ADDRESS_LEN] = [
    0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x44,
];

// ── Protocol limits ───────────────────────────────────────────────────
//
// From `arkiv-engine.md` §2. Unlike a node's page-size policy, these are part of
// what a transaction means: an operation over one of them is invalid everywhere,
// so the ABI encoder, the query parser and the engine all check the same number.

/// The longest `str` value, in bytes.
///
/// Enforced by the ABI encoder before a transaction is sent, by the query parser
/// on a `str(…)` literal, and by the engine on decode. The index's string cascade
/// is sized to hold exactly this much (four [`WORD_LEN`] chunks).
pub const MAX_STR_BYTES: usize = 128;

/// The longest `$payload` (`bytes`) value, in bytes.
///
/// Payloads are stored whole and are never indexed, so this is the one limit that
/// bounds an entity's size rather than its shape.
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

/// The most attributes one entity operation may carry.
pub const MAX_ATTRIBUTES: usize = 32;

/// The longest attribute name, in bytes — the width the ABI's `Ident32` carries.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 32;

// ── Invariants ────────────────────────────────────────────────────────
//
// Checked at compile time — a constants module's guarantees belong here, not in
// runtime tests.

/// The widths match the arrays they name.
const _: () = assert!(WORD_LEN == size_of::<[u8; WORD_LEN]>());
const _: () = assert!(ETH_ADDRESS_LEN == size_of::<[u8; ETH_ADDRESS_LEN]>());
const _: () = assert!(ETH_ADDRESS_LEN == ARKIV_ADDRESS.len());

/// An Ethereum address is right-aligned into a word by the ABI encoder, so it has
/// to fit in one.
///
/// Note what this is *not*: a statement that an Arkiv address is narrower than a
/// word. Arkiv addresses in [`WORD_LEN`] — this bounds only the host's account-key
/// width, which is why it is written against [`ETH_ADDRESS_LEN`] and not against
/// some general notion of "address".
const _: () = assert!(ETH_ADDRESS_LEN <= WORD_LEN);

/// An attribute name is exactly one word wide, which is what lets the ABI carry it
/// as a `bytes32` UDVT.
const _: () = assert!(MAX_ATTRIBUTE_NAME_BYTES == WORD_LEN);

/// A `str` value is a whole number of words, so the index's cascade splits it into
/// chunks with nothing left over.
const _: () = assert!(MAX_STR_BYTES.is_multiple_of(WORD_LEN));
