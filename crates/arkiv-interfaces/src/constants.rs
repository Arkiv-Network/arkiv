//! Constants more than one crate has to agree on.

pub mod ethereum {
    pub const EVM_WORD_LENGTH: usize = 32;
    pub const ETH_ADDRESS_LEN: usize = 20;

    /// The Arkiv address — `0x4400…0044`, where entity calldata is sent.
    pub const ARKIV_RETH_ADDRESS: [u8; ETH_ADDRESS_LEN] = [
        0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x44,
    ];
}

/// The longest `str` value in bytes, allowed in arkiv.
pub const MAX_STR_BYTES: usize = 128;

/// The longest `$payload` in bytes, allowed in arkiv.
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

/// The most attributes one entity operation may carry.
pub const MAX_ATTRIBUTES: usize = 32;

/// The longest attribute name — the width the ABI's `Ident32` carries.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 32;

// Invariants: Compile-time checks

/// An attribute name is one word wide, which is what lets the ABI carry it as a
/// `bytes32` UDVT.
const _: () = assert!(MAX_ATTRIBUTE_NAME_BYTES <= ethereum::EVM_WORD_LENGTH);

/// A `str` splits into whole cascade chunks with nothing left over.
const _: () = assert!(MAX_STR_BYTES.is_multiple_of(ethereum::EVM_WORD_LENGTH));
