//! Rust bindings for the Arkiv `IEntityRegistry` ABI — the node's write
//! surface, and this crate is its source of truth.
//!
//! The shape here is the **frozen client surface** specified in
//! `arkiv-node-api.md` §3. The `arkiv-contracts` `IEntityRegistry.sol` this was
//! originally vendored from is retired — the node answers calls to
//! `ARKIV_ADDRESS` directly — so the spec document, not a compiled artifact, is
//! what these types track.
//!
//! Two shapes carry the whole write path:
//!
//! - [`Operation`] is a **tagged union**: `operation` selects which payload
//!   struct ([`Create`], [`Patch`], [`ExtendExpiry`], [`TransferOwnership`],
//!   [`Delete`]) the `operationData` bytes ABI-decode to. New op types can be
//!   added without changing `execute`'s signature.
//! - [`Attribute`] is one `(name, typeId, value)` triple, shared by `create`'s
//!   attributes and `patch`'s mutations. Its `value` is variable-length
//!   `bytes`, so the same list carries a `bool` and a 128 KiB `$payload`.
//!   `typeId = 0` is a [tombstone](arkiv_interfaces::entity::TOMBSTONE_TYPE_ID)
//!   — "unset this attribute" — valid only in `patch`.
//!
//! Alongside the `sol!` block, [`encode`] holds the constructors and the
//! `AttributeValue` ↔ wire mapping, and `types` the `Ident32` name validation.
//!
//! [`tests::selectors_are_pinned`] pins the function selectors, so a
//! transcription error here — or an unnoticed field reordering, which changes
//! the selector — fails loudly rather than silently forking the wire.

pub mod encode;
pub mod types;

alloy_sol_types::sol! {
    #[derive(Debug, PartialEq, Eq, Hash)]
    type Ident32 is bytes32;

    /// The one wire shape shared by `create`'s attributes and `patch`'s
    /// mutations. `value` is variable-length `bytes` (not a fixed word array),
    /// which is what lets `$payload` ride in the same list as a `bool`.
    #[derive(Debug, PartialEq, Eq)]
    struct Attribute {
        Ident32 name;
        uint8 typeId;
        bytes value;
    }

    /// Tagged union: `operation` selects the payload struct that
    /// `operationData` ABI-decodes to. New op types extend the protocol
    /// without changing `execute`'s signature.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Operation {
        uint8 operation;
        bytes operationData;
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Create {
        uint128 salt;
        uint64 expiresAt;
        uint64 minLifetime;
        uint8 creationFlags;
        Attribute[] attributes;
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Patch {
        bytes32 entityKey;
        Attribute[] mutations;
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct ExtendExpiry {
        bytes32 entityKey;
        uint64 expiresAt;
        uint64 minLifetime;
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct TransferOwnership {
        bytes32 entityKey;
        address newOwner;
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Delete {
        bytes32 entityKey;
    }

    #[sol(rpc)]
    interface IEntityRegistry {
        function execute(Operation[] ops) external returns (bytes32[] keys);
        function purgeExpired(bytes32[] entityKeys) external;
        function entityNonce(address owner) external view returns (uint64);
        function customAttributeNames(bytes32 entityKey) external view returns (Ident32[] names);
        function attributeTypeId(bytes32 entityKey, Ident32 name) external view returns (uint8 typeId);
        event EntityCreated(bytes32 indexed entityKey, address indexed owner, uint64 expiresAt, uint8 creationFlags);
        event EntityPatched(bytes32 indexed entityKey, address indexed owner);
        event ExpiryExtended(bytes32 indexed entityKey, address indexed owner, uint64 expiresAt);
        event OwnershipTransferred(bytes32 indexed entityKey, address indexed previousOwner, address indexed newOwner);
        event EntityDeleted(bytes32 indexed entityKey, address indexed owner);
        error AttributesNotSorted();
        error EmptyBatch();
        error EmptyMutations(bytes32 entityKey);
        error EntityExpired(bytes32 entityKey, uint64 expiresAt);
        error EntityNotFound(bytes32 entityKey);
        error ExpiryNotExtended(bytes32 entityKey, uint64 newExpiresAt, uint64 currentExpiresAt);
        error ExpiryDeadOnArrival(uint64 target, uint64 currentBlock);
        error InvalidOpType(uint8 operation);
        error InvalidValueType(Ident32 name, uint8 typeId);
        error NonCanonicalOperationData(uint8 operation);
        error NotOwner(bytes32 entityKey, address caller, address owner);
        error ReadOnlyEntity(bytes32 entityKey);
        error ReservedCreationFlags(uint8 creationFlags);
        error SystemAttributeNotWritable(Ident32 name);
        error TombstoneInCreate(Ident32 name);
        error TombstoneValueNotEmpty(Ident32 name);
        error TooManyAttributes(uint256 count, uint256 maxCount);
        error TransferToSelf(bytes32 entityKey);
        error TransferToZeroAddress(bytes32 entityKey);
        // Attribute-name validation errors, part of the node ABI the SDK decodes.
        error Ident32Empty();
        error Ident32InvalidByte(uint256 position, bytes1 value);
    }
}

/// Operation tags — the `operation` byte selecting `operationData`'s struct.
///
/// The `expire` op is deliberately absent: expiry is protocol-driven, purged by
/// a per-block system call rather than a client-submitted operation
/// (`arkiv-engine.md` §5).
pub const OP_CREATE: u8 = 1;
pub const OP_PATCH: u8 = 2;
pub const OP_EXTEND_EXPIRY: u8 = 3;
pub const OP_TRANSFER_OWNERSHIP: u8 = 4;
pub const OP_DELETE: u8 = 5;

/// Maximum number of expired entities carried by one protocol purge transaction.
pub const MAX_PURGE_KEYS: usize = 10;
/// Soft cumulative gas threshold for one block's purge selection.
pub const PURGE_GAS_THRESHOLD: u64 = 1_000_000;

/// Creation flags. Like the `typeId`s they belong to the protocol rather than
/// the ABI, so the type lives in the spec crate and is re-exported here.
pub use arkiv_interfaces::entity::CreationFlags;

/// The attribute type set. The `typeId`s belong to the protocol, not the ABI, so
/// they live in the spec crate and are re-exported here rather than mirrored.
pub use arkiv_interfaces::entity::{AttributeType, AttributeValue, DECIMAL_SCALE};

/// Attribute-count limits for an operation and for the resulting entity. The
/// engine rejects either excess with `TooManyAttributes`; SDKs can validate
/// locally before sending a transaction. Protocol limits live in the spec crate
/// and are re-exported here rather than mirrored.
pub use arkiv_interfaces::constants::{ENTITY_MAX_ATTRIBUTES, OP_MAX_ATTRIBUTES};

/// Human-readable label for an operation tag, mirroring the `OP_*` constants.
/// Returns `"UNKNOWN"` for any unrecognised discriminator.
pub fn op_type_name(op_type: u8) -> &'static str {
    match op_type {
        OP_CREATE => "CREATE",
        OP_PATCH => "PATCH",
        OP_EXTEND_EXPIRY => "EXTEND_EXPIRY",
        OP_TRANSFER_OWNERSHIP => "TRANSFER_OWNERSHIP",
        OP_DELETE => "DELETE",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;

    /// The pinned selectors, recomputed independently of this crate:
    ///
    /// ```text
    /// cast sig 'execute((uint8,bytes)[])'          # 0x49650044
    /// cast sig 'entityNonce(address)'              # 0x36917bfd
    /// cast sig 'customAttributeNames(bytes32)'     # 0x58d5418a
    /// cast sig 'attributeTypeId(bytes32,bytes32)'  # 0x434fb6f3
    /// ```
    ///
    /// The `Ident32` parameter contributes its **underlying** `bytes32` to the
    /// signature, not its UDVT name — which is why the last one reads
    /// `(bytes32,bytes32)`.
    const EXECUTE_SELECTOR: [u8; 4] = [0x49, 0x65, 0x00, 0x44];
    const ENTITY_NONCE_SELECTOR: [u8; 4] = [0x36, 0x91, 0x7b, 0xfd];
    const CUSTOM_ATTRIBUTE_NAMES_SELECTOR: [u8; 4] = [0x58, 0xd5, 0x41, 0x8a];
    const ATTRIBUTE_TYPE_ID_SELECTOR: [u8; 4] = [0x43, 0x4f, 0xb6, 0xf3];

    /// Wire constants shared with the SDK — they must never drift silently.
    ///
    /// A selector is `keccak256(canonicalSignature)[..4]` and a struct parameter
    /// contributes its *flattened tuple*, so reordering or retyping any field of
    /// [`Operation`] changes these. The check is independent: the constants above
    /// came from `cast`, the values here from what `alloy` derives from the `sol!`
    /// block, and nothing in this file feeds both.
    #[test]
    fn selectors_are_pinned() {
        assert_eq!(IEntityRegistry::executeCall::SELECTOR, EXECUTE_SELECTOR);
        assert_eq!(
            IEntityRegistry::entityNonceCall::SELECTOR,
            ENTITY_NONCE_SELECTOR
        );
        assert_eq!(
            IEntityRegistry::customAttributeNamesCall::SELECTOR,
            CUSTOM_ATTRIBUTE_NAMES_SELECTOR
        );
        assert_eq!(
            IEntityRegistry::attributeTypeIdCall::SELECTOR,
            ATTRIBUTE_TYPE_ID_SELECTOR
        );
    }

    /// A selector is `keccak256(canonicalSignature)[..4]` — so the pins can be
    /// re-derived here too, from the signature strings, with no external tool.
    /// This is what makes the `cast` lines above reproducible rather than
    /// folklore.
    #[test]
    fn pinned_selectors_match_their_signatures() {
        let selector_of = |signature: &str| -> [u8; 4] {
            alloy_primitives::keccak256(signature.as_bytes())[..4]
                .try_into()
                .unwrap()
        };
        assert_eq!(selector_of("execute((uint8,bytes)[])"), EXECUTE_SELECTOR);
        assert_eq!(selector_of("entityNonce(address)"), ENTITY_NONCE_SELECTOR);
        assert_eq!(
            selector_of("customAttributeNames(bytes32)"),
            CUSTOM_ATTRIBUTE_NAMES_SELECTOR
        );
        assert_eq!(
            selector_of("attributeTypeId(bytes32,bytes32)"),
            ATTRIBUTE_TYPE_ID_SELECTOR
        );
    }

    /// The op tags are consensus constants: contiguous from 1, and with no
    /// `expire` — expiry is the protocol's job, not a client op.
    #[test]
    fn op_tags_are_pinned() {
        assert_eq!(
            [
                OP_CREATE,
                OP_PATCH,
                OP_EXTEND_EXPIRY,
                OP_TRANSFER_OWNERSHIP,
                OP_DELETE
            ],
            [1, 2, 3, 4, 5]
        );
        assert_eq!(op_type_name(6), "UNKNOWN");
    }

    /// Reserved bits stay reserved: the mask is exactly the two V1 flags, so a
    /// client setting bits 2–7 is rejected rather than silently accepted.
    #[test]
    fn creation_flag_mask_covers_only_the_v1_flags() {
        assert_eq!(CreationFlags::READONLY.bits(), 0b0000_0001);
        assert_eq!(CreationFlags::PERMISSIONLESS_EXTENSION.bits(), 0b0000_0010);
        assert_eq!(CreationFlags::MASK, 0b0000_0011);
        // Every reserved bit is refused at the door.
        for bit in 2..8 {
            assert_eq!(CreationFlags::from_bits(1 << bit), None, "bit {bit}");
        }
        assert_eq!(
            CreationFlags::from_bits(0b11),
            Some(CreationFlags::READONLY | CreationFlags::PERMISSIONLESS_EXTENSION)
        );
    }
}
