//! Constants more than one crate has to agree on.

/// A 256-bit EVM word — also a hash, a commitment root, an entity address, and
/// the width Arkiv addresses its own derived locations in.
pub const EVM_WORD_LENGTH: usize = 32;

/// Width of an **Ethereum account** address — a property of the host binding, not
/// of Arkiv, whose own address space is [`EVM_WORD_LENGTH`] wide.
pub const ETH_ADDRESS_LEN: usize = 20;

/// The Arkiv address — `0x4400…0044`, where entity calldata is sent.
///
/// No bytecode and no precompile object: the executor routes calls here directly,
/// so genesis carries no allocation. Raw bytes because this crate names no
/// external types; a host wraps it in its own address type at the boundary.
pub const ARKIV_RETH_ADDRESS: [u8; ETH_ADDRESS_LEN] = [
    0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x44,
];

/// The longest `str` value in bytes, allowed in protocol.
pub const MAX_STR_BYTES: usize = 128;

/// The longest `$payload`. Payloads are stored whole and never indexed, so this
/// is the one limit bounding an entity's size rather than its shape.
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

/// The most attributes one entity operation may carry.
pub const MAX_ATTRIBUTES: usize = 32;

/// The longest attribute name — the width the ABI's `Ident32` carries.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 32;

// ── Invariants ────────────────────────────────────────────────────────

const _: () = assert!(EVM_WORD_LENGTH == size_of::<[u8; EVM_WORD_LENGTH]>());
const _: () = assert!(ETH_ADDRESS_LEN == size_of::<[u8; ETH_ADDRESS_LEN]>());
const _: () = assert!(ETH_ADDRESS_LEN == ARKIV_RETH_ADDRESS.len());

/// The ABI right-aligns an Ethereum address into a word. Deliberately *not* a
/// claim that an Arkiv address is narrower than a word — it isn't.
const _: () = assert!(ETH_ADDRESS_LEN <= EVM_WORD_LENGTH);

/// An attribute name is one word wide, which is what lets the ABI carry it as a
/// `bytes32` UDVT.
const _: () = assert!(MAX_ATTRIBUTE_NAME_BYTES == EVM_WORD_LENGTH);

/// A `str` splits into whole cascade chunks with nothing left over.
const _: () = assert!(MAX_STR_BYTES.is_multiple_of(EVM_WORD_LENGTH));
