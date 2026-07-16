//! Vendored Rust bindings for the Arkiv `IEntityRegistry` ABI — trimmed to
//! the surface the no-EVM node actually implements.
//!
//! Originally vendored from `IEntityRegistry.sol` in
//! <https://github.com/Arkiv-Network/arkiv-contracts> (rev `d6ebe18`), which
//! this crate replaces as the ABI's source of truth: the contract itself is
//! retired, and the node answers calls to `ARKIV_ADDRESS` directly. Only the
//! functions the node dispatches are kept — `execute(Operation[])` and
//! `nonces(address)` — plus the `EntityOperation` event the executor emits
//! and the business-rule errors. The contract's other view functions
//! (`commitment`, `entityKey`, `changeSetHash*`, block-node walking, …) were
//! dropped along with their return structs; the node never implemented them.
//!
//! Alongside the `sol!` block, three upstream modules are vendored verbatim
//! (minus their `serde-wire` feature gates, which nothing here uses):
//! [`encode`] (the `Operation` / `Attribute` constructors) and the
//! `Ident32` / `Mime128` validation impls in `types`.
//!
//! [`tests::selectors_match_compiled_abi`] pins every function selector to the
//! values from the compiled artifact's `methodIdentifiers`, so a transcription
//! error here (or an upstream ABI change on re-sync) fails loudly.

pub mod encode;
pub mod types;

alloy_sol_types::sol! {
    #[derive(Debug, PartialEq, Eq, Hash)]
    type BlockNumber32 is uint32;
    #[derive(Debug, PartialEq, Eq, Hash)]
    type Ident32 is bytes32;

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Mime128 {
        bytes32[4] data;
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Attribute {
        Ident32 name;
        uint8 valueType;
        bytes32[4] value;
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        BlockNumber32 btl;
        address newOwner;
    }

    #[sol(rpc)]
    interface IEntityRegistry {
        function execute(Operation[] ops) external;
        function nonces(address owner) external view returns (uint32);
        event EntityOperation(bytes32 indexed entityKey, uint8 indexed operationType, address indexed owner, BlockNumber32 expiresAt, bytes32 entityHash);
        error AttributesNotSorted();
        error EmptyBatch();
        error EntityExpired(bytes32 entityKey, BlockNumber32 expiresAt);
        error EntityNotExpired(bytes32 entityKey, BlockNumber32 expiresAt);
        error EntityNotFound(bytes32 entityKey);
        error ExpiryNotExtended(bytes32 entityKey, BlockNumber32 newExpiresAt, BlockNumber32 currentExpiresAt);
        error InvalidOpType(uint8 operationType);
        error InvalidValueType(Ident32 name, uint8 valueType);
        error NotOwner(bytes32 entityKey, address caller, address owner);
        error TooManyAttributes(uint256 count, uint256 maxCount);
        error TransferToSelf(bytes32 entityKey);
        error TransferToZeroAddress(bytes32 entityKey);
        error ZeroBtl();
        // Attribute-name validation errors. Not in the d6ebe18 compiled
        // artifact, but part of the node ABI the SDK decodes (declared by the
        // engine's inline interface on kf/merge-db-engine-into-custom-exec).
        error Ident32Empty();
        error Ident32InvalidByte(uint256 position, bytes1 value);
    }
}

/// Operation type constants (mirrors Entity.sol).
pub const OP_CREATE: u8 = 1;
pub const OP_UPDATE: u8 = 2;
pub const OP_EXTEND: u8 = 3;
pub const OP_TRANSFER: u8 = 4;
pub const OP_DELETE: u8 = 5;
pub const OP_EXPIRE: u8 = 6;

/// Attribute value type constants (mirrors Entity.sol).
pub const ATTR_UINT: u8 = 1;
pub const ATTR_STRING: u8 = 2;
pub const ATTR_ENTITY_KEY: u8 = 3;

/// Maximum number of attributes per entity operation (mirrors Entity.sol's
/// internal `MAX_ATTRIBUTES`). The contract reverts `TooManyAttributes` past
/// this count; SDKs can validate locally before sending a transaction.
pub const MAX_ATTRIBUTES: usize = 32;

/// Human-readable label for an operation type, mirroring the `OP_*`
/// constants. Returns `"UNKNOWN"` for any unrecognised discriminator.
pub fn op_type_name(op_type: u8) -> &'static str {
    match op_type {
        OP_CREATE => "CREATE",
        OP_UPDATE => "UPDATE",
        OP_EXTEND => "EXTEND",
        OP_TRANSFER => "TRANSFER",
        OP_DELETE => "DELETE",
        OP_EXPIRE => "EXPIRE",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;

    /// Selectors as reported by the compiled artifact's `methodIdentifiers`
    /// (arkiv-contracts rev d6ebe18,
    /// `out/IEntityRegistry.sol/IEntityRegistry.json`). These are wire
    /// constants shared with the SDK — they must never drift.
    #[test]
    fn selectors_match_compiled_abi() {
        assert_eq!(
            IEntityRegistry::executeCall::SELECTOR,
            [0xba, 0x8c, 0xcf, 0x92]
        );
        assert_eq!(
            IEntityRegistry::noncesCall::SELECTOR,
            [0x7e, 0xce, 0xbe, 0x00]
        );
    }
}
