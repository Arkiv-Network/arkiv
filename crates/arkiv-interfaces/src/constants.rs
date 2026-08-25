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

/// The JSON-RPC error codes the `arkiv_*` methods answer with.
pub mod rpc_error_codes {
    /// Malformed query text: an unexpected token, an unclosed group, trailing
    /// junk.
    pub const MALFORMED_INPUT: i32 = -32001;

    /// Well-formed but not well-typed: a range operator on an equality-only
    /// type, an unknown type tag, a value whose type doesn't fit the attribute.
    pub const TYPE_ERROR: i32 = -32002;

    /// A literal that doesn't fit its tag: `i32` out of range, a bad EIP-55
    /// checksum, more than 18 decimal places, an over-long string.
    pub const LITERAL_ERROR: i32 = -32003;

    /// The query is too big: too long, too many predicates, nested too deeply.
    pub const QUERY_LIMIT: i32 = -32004;

    /// A cursor that is malformed, or bound to a different request.
    pub const CURSOR_ERROR: i32 = -32005;

    /// An `atBlock` outside the node's retained range.
    pub const BLOCK_UNAVAILABLE: i32 = -32006;

    /// Every code above, in allocation order.
    pub const ALL: [i32; 6] = [
        MALFORMED_INPUT,
        TYPE_ERROR,
        LITERAL_ERROR,
        QUERY_LIMIT,
        CURSOR_ERROR,
        BLOCK_UNAVAILABLE,
    ];

    // Invariant: no two codes collide. This is the reason the list is
    // centralized — a duplicate is a build failure, not a client that silently
    // mistakes one failure for another.
    const _: () = {
        let mut i = 0;
        while i < ALL.len() {
            let mut j = i + 1;
            while j < ALL.len() {
                assert!(ALL[i] != ALL[j], "two arkiv RPC error codes collide");
                j += 1;
            }
            i += 1;
        }
    };
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
