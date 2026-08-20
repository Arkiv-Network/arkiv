//! Constants more than one crate has to agree on.
//!
//! Byte widths the encodings are built on, and the protocol limits the ABI
//! encoder, the query parser and the engine all check. Each limit here previously
//! had two or three independent definitions — a cap the SDK enforces at 128 and
//! the engine at 129 is a consensus split, not a typo.
//!
//! A value only one crate reads stays in that crate.

// ── Byte widths ───────────────────────────────────────────────────────

/// A 256-bit EVM word — also a hash, a commitment root, an entity key, and the
/// width Arkiv addresses its own derived locations in.
pub const WORD_LEN: usize = 32;

/// Width of an **Ethereum account** address — a property of the host binding, not
/// of Arkiv, whose own address space is [`WORD_LEN`] wide.
///
/// Two unrelated things used to share this name, and telling them apart is the
/// reason for the `ETH_` prefix:
///
/// - Really an Ethereum account, where 20 is fixed by Ethereum: `owner`,
///   `creator`, the `addr` type and its EIP-55 checksums, [`ARKIV_ADDRESS`], the
///   ABI `address` word.
/// - A 32-byte Arkiv value truncated to fit reth's account key, dropping 96 bits:
///   every index address in `arkiv-reth-mpt-committed-store`, and
///   `entity_address`. Those hold
///   no keys and no signer — they are storage locations that must wear an address
///   because reth keys accounts by one.
pub const ETH_ADDRESS_LEN: usize = 20;

/// The Arkiv address — `0x4400…0044`, where entity calldata is sent.
///
/// No bytecode and no precompile object: the executor routes calls here directly,
/// so genesis carries no allocation. Raw bytes because this crate names no
/// external types; hosts wrap it as `Address::new(ARKIV_ADDRESS)`.
pub const ARKIV_ADDRESS: [u8; ETH_ADDRESS_LEN] = [
    0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x44,
];

// ── Protocol limits (`arkiv-engine.md` §2) ────────────────────────────

/// The longest `str` value. The index's string cascade is sized to hold exactly
/// this much.
pub const MAX_STR_BYTES: usize = 128;

/// The longest `$payload`. Payloads are stored whole and never indexed, so this
/// is the one limit bounding an entity's size rather than its shape.
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

/// The most attributes one entity operation may carry.
pub const MAX_ATTRIBUTES: usize = 32;

/// The longest attribute name — the width the ABI's `Ident32` carries.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 32;

// ── Invariants ────────────────────────────────────────────────────────

const _: () = assert!(WORD_LEN == size_of::<[u8; WORD_LEN]>());
const _: () = assert!(ETH_ADDRESS_LEN == size_of::<[u8; ETH_ADDRESS_LEN]>());
const _: () = assert!(ETH_ADDRESS_LEN == ARKIV_ADDRESS.len());

/// The ABI right-aligns an Ethereum address into a word. Deliberately *not* a
/// claim that an Arkiv address is narrower than a word — it isn't.
const _: () = assert!(ETH_ADDRESS_LEN <= WORD_LEN);

/// An attribute name is one word wide, which is what lets the ABI carry it as a
/// `bytes32` UDVT.
const _: () = assert!(MAX_ATTRIBUTE_NAME_BYTES == WORD_LEN);

/// A `str` splits into whole cascade chunks with nothing left over.
const _: () = assert!(MAX_STR_BYTES.is_multiple_of(WORD_LEN));
