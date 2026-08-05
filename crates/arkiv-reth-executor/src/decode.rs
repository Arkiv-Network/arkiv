//! ABI op decoding — the host's calldata → [`Op`] step.
//!
//! A client `CALL`s the Arkiv address with `execute(Operation[])` calldata (the
//! `IEntityRegistry` ABI, from `arkiv-bindings`). [`decode_ops`] turns those raw
//! bytes into the spec's host-agnostic [`Op`]s that [`ArkivExecutor::apply`] runs.
//! All ABI knowledge lives here; the executor never sees Solidity types.
//!
//! Each [`Operation`] is a tagged union — an `operation` byte plus an
//! `operationData` blob that ABI-decodes to the struct that tag names. The blob
//! must be *canonically* encoded (`arkiv-node-api.md` §3), so one op has exactly
//! one spelling on the wire.
//!
//! Three things are *resolved* here, per the [`Op`] contract:
//!
//! - **Expiry.** The ABI carries `(expiresAt, minLifetime)`; the [`Op`] carries
//!   one absolute block, `max(expiresAt, current + minLifetime)`, which must be
//!   in the future. See [`resolve_expiry`].
//! - **The create key.** A `Create` carries no key — the node mints it from
//!   `(domain, owner, nonce, salt)`; see [`derive_entity_key`]. A batch shares
//!   one caller, so successive creates use `start_nonce`, `start_nonce + 1`, …;
//!   reading and advancing the caller's persistent nonce is the wiring step's
//!   job, which is why `start_nonce` is a parameter and decode stays a pure
//!   function.
//! - **`$payload` / `$contentType`.** They travel as ordinary triples in the
//!   attribute list; a `Create` lifts them out into the entity's own fields,
//!   since a create always yields a whole entity. A `Patch` leaves them in its
//!   mutation list — a patch is a delta, and the executor routes them there.
//!
//! What decode does *not* do is check state: ownership, liveness, and whether an
//! entity exists are the executor's business. Everything rejected here is a
//! structural fault, decidable from the calldata alone.
//!
//! [`ArkivExecutor::apply`]: crate::ArkivExecutor

use core::fmt;

use alloy_primitives::{Address, U256, keccak256};
use alloy_sol_types::SolCall;
use arkiv_bindings::{
    Attribute as AbiAttribute, Create, Delete, ExtendExpiry, IEntityRegistry, MAX_ATTRIBUTES,
    OP_CREATE, OP_DELETE, OP_EXTEND_EXPIRY, OP_PATCH, OP_TRANSFER_OWNERSHIP, Operation, Patch,
    TransferOwnership,
    encode::{AttrAbiError, OpAbiError},
    types::{Ident32ByteError, validate_ident32_bytes, validate_system_ident32_bytes},
};
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags, annotations};
use arkiv_interfaces::execution::{AttributeMutation, ExecEnv, Op};
use arkiv_interfaces::primitives::{BlockNumber, EntityKey, EntityNonce};

use arkiv_constants::{ADDRESS_LEN, WORD_LEN};

use crate::ARKIV_ADDRESS;

/// Decode `execute(Operation[])` calldata into the batch's [`Op`]s.
///
/// `start_nonce` is the caller's entity-key minting nonce at the start of the
/// batch; the `i`-th `Create` in the batch mints its key from `start_nonce + i`.
pub fn decode_ops(
    env: &ExecEnv,
    calldata: &[u8],
    start_nonce: EntityNonce,
) -> Result<Vec<Op>, DecodeError> {
    if calldata.len() < 4 {
        return Err(DecodeError::CalldataTooShort);
    }
    let selector: [u8; 4] = calldata[..4].try_into().unwrap();
    if selector != IEntityRegistry::executeCall::SELECTOR {
        return Err(DecodeError::UnknownSelector(selector));
    }
    let batch = IEntityRegistry::executeCall::abi_decode_raw(&calldata[4..])
        .map_err(|e| DecodeError::Abi(e.to_string()))?;
    if batch.ops.is_empty() {
        return Err(DecodeError::EmptyBatch);
    }

    let mut out = Vec::with_capacity(batch.ops.len());
    let mut create_index: u64 = 0;
    for op in &batch.ops {
        let decoded = match op.operation {
            OP_CREATE => {
                let c: Create = payload(op)?;
                let Some(creation_flags) = CreationFlags::from_bits(c.creationFlags) else {
                    return Err(DecodeError::ReservedCreationFlags(c.creationFlags));
                };
                let nonce = start_nonce.advanced_by(create_index);
                create_index += 1;
                let SystemSplit {
                    content_type,
                    payload,
                    attributes,
                } = split_create_attributes(&c.attributes)?;
                Op::Create {
                    key: derive_entity_key(env.chain_id, &env.caller, nonce, c.salt),
                    expires_at: resolve_expiry(env.block_number, c.expiresAt, c.minLifetime)?,
                    creation_flags,
                    content_type,
                    payload,
                    attributes,
                }
            }
            OP_PATCH => {
                let p: Patch = payload(op)?;
                if p.mutations.is_empty() {
                    return Err(DecodeError::EmptyMutations { key: p.entityKey.0 });
                }
                Op::Patch {
                    key: p.entityKey.0,
                    mutations: convert_mutations(&p.mutations)?,
                }
            }
            OP_EXTEND_EXPIRY => {
                let e: ExtendExpiry = payload(op)?;
                Op::ExtendExpiry {
                    key: e.entityKey.0,
                    new_expires_at: resolve_expiry(env.block_number, e.expiresAt, e.minLifetime)?,
                }
            }
            OP_TRANSFER_OWNERSHIP => {
                let t: TransferOwnership = payload(op)?;
                if t.newOwner == Address::ZERO {
                    return Err(DecodeError::TransferToZeroAddress { key: t.entityKey.0 });
                }
                Op::Transfer {
                    key: t.entityKey.0,
                    new_owner: t.newOwner.into_array(),
                }
            }
            OP_DELETE => {
                let d: Delete = payload(op)?;
                Op::Delete { key: d.entityKey.0 }
            }
            other => return Err(DecodeError::InvalidOpType(other)),
        };
        out.push(decoded);
    }
    Ok(out)
}

/// Decode one op's `operationData` as the struct its tag names.
fn payload<T>(op: &Operation) -> Result<T, DecodeError>
where
    T: alloy_sol_types::SolValue
        + From<<<T as alloy_sol_types::SolValue>::SolType as alloy_sol_types::SolType>::RustType>,
{
    op.payload_of::<T>()
        .map_err(|e| DecodeError::OperationData {
            operation: op.operation,
            reason: e,
        })
}

/// Resolve the two wire expiry args into one absolute block.
///
/// `target = max(expiresAt, current + minLifetime)`, which covers absolute
/// (`minLifetime = 0`), relative (`expiresAt = 0`), and "absolute with a floor"
/// in a single rule — the floor raises a stale or too-close `expiresAt` instead
/// of silently under-delivering the lifetime asked for.
///
/// The result must be strictly in the future: an op that would mint or extend to
/// an already-dead block is a client error, not a no-op.
pub fn resolve_expiry(
    current: BlockNumber,
    expires_at: u64,
    min_lifetime: u64,
) -> Result<BlockNumber, DecodeError> {
    // Checked, so an absurd floor reverts rather than wrapping. Permanence is
    // expressed as `expiresAt = u64::MAX`, never as a huge `minLifetime`.
    let floor = current
        .checked_add(min_lifetime)
        .ok_or(DecodeError::ExpiryOverflow {
            current,
            min_lifetime,
        })?;
    let target = expires_at.max(floor);
    if target <= current {
        return Err(DecodeError::ExpiryDeadOnArrival { target, current });
    }
    Ok(target)
}

/// Mint a `Create`'s entity key: `keccak256(domain || owner || nonce || salt)`.
///
/// The **domain** is this chain's `(chain_id, ARKIV_ADDRESS)` pair, which keeps
/// keys from colliding across chains. `nonce` is the caller's per-owner entity
/// counter and is what makes the key *unique*; `salt` is caller-chosen and only
/// makes it *unpredictable*. Without a salt the next key is a pure function of
/// public state, so anyone could compute a creator's future key and pre-entangle
/// it — front-loaded reference spam aimed at an entity that does not exist yet.
/// `salt = 0` is valid and yields a unique but publicly predictable key; SDKs
/// default it to 128 random bits.
///
/// This must match the SDK's local derivation exactly — a client predicts the
/// key it is about to create, and a later op in the same batch targets it.
pub fn derive_entity_key(
    chain_id: u64,
    owner: &[u8; 20],
    nonce: EntityNonce,
    salt: u128,
) -> EntityKey {
    // chain_id ‖ ARKIV_ADDRESS ‖ owner ‖ nonce ‖ salt
    let mut buf = Vec::with_capacity(
        WORD_LEN + ADDRESS_LEN + ADDRESS_LEN + size_of::<u64>() + size_of::<u128>(),
    );
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(ARKIV_ADDRESS.as_slice());
    buf.extend_from_slice(owner.as_slice());
    buf.extend_from_slice(&nonce.get().to_be_bytes());
    buf.extend_from_slice(&salt.to_be_bytes());
    keccak256(&buf).0
}

/// A create's attribute list, split into the entity's system fields and its
/// user attributes.
struct SystemSplit {
    content_type: Vec<u8>,
    payload: Vec<u8>,
    attributes: Vec<Attribute>,
}

/// Validate a `create`'s attribute list and lift `$payload` / `$contentType`
/// out of it.
///
/// Tombstones are rejected: on a fresh entity there is nothing to unset, so
/// "absent" is said by omission — one canonical way to encode it.
fn split_create_attributes(attrs: &[AbiAttribute]) -> Result<SystemSplit, DecodeError> {
    check_triple_list(attrs)?;
    let mut out = SystemSplit {
        content_type: Vec::new(),
        payload: Vec::new(),
        attributes: Vec::with_capacity(attrs.len()),
    };
    for a in attrs {
        let name = attr_name(a);
        let Some(value) = decode_attr_value(a)? else {
            return Err(DecodeError::TombstoneInCreate { name });
        };
        match name.as_slice() {
            annotations::PAYLOAD => out.payload = value.encode(),
            annotations::CONTENT_TYPE => out.content_type = value.encode(),
            _ => out.attributes.push(Attribute { key: name, value }),
        }
    }
    Ok(out)
}

/// Validate a `patch`'s mutation list. Tombstones are what make it a patch, so
/// they are kept — `None` is "unset this".
fn convert_mutations(attrs: &[AbiAttribute]) -> Result<Vec<AttributeMutation>, DecodeError> {
    check_triple_list(attrs)?;
    attrs
        .iter()
        .map(|a| {
            Ok(AttributeMutation {
                key: attr_name(a),
                value: decode_attr_value(a)?,
            })
        })
        .collect()
}

/// The structural rules every triple list obeys, whichever op carries it: at
/// most [`MAX_ATTRIBUTES`], valid `Ident32` names, strictly ascending by name
/// (which also enforces uniqueness), and no engine-controlled `$` name.
fn check_triple_list(attrs: &[AbiAttribute]) -> Result<(), DecodeError> {
    if attrs.len() > MAX_ATTRIBUTES {
        return Err(DecodeError::TooManyAttributes {
            count: attrs.len(),
            max: MAX_ATTRIBUTES,
        });
    }
    for a in attrs {
        let name = attr_name(a);
        // System names (`$…`) are protocol-defined and exempt from the `a-z`
        // leading-byte rule for user attributes — `$` is the byte that
        // separates the two namespaces. Which of them a client may write is
        // the allow-list's call, checked first so the error names the real
        // problem ("the engine owns this") rather than a charset violation.
        let validate = if name.first() == Some(&annotations::SYSTEM_PREFIX) {
            if annotations::is_engine_controlled(&name) {
                return Err(DecodeError::SystemAttributeNotWritable { name });
            }
            validate_system_ident32_bytes
        } else {
            validate_ident32_bytes
        };
        if let Err(e) = validate(&a.name.0) {
            return Err(match e {
                Ident32ByteError::Empty => DecodeError::AttributeNameEmpty,
                Ident32ByteError::InvalidByte { position, value } => {
                    DecodeError::AttributeNameInvalidByte { position, value }
                }
            });
        }
    }
    if attrs.windows(2).any(|w| w[0].name.0 >= w[1].name.0) {
        return Err(DecodeError::AttributesNotSorted);
    }
    Ok(())
}

/// One triple's value: `None` for a tombstone.
fn decode_attr_value(a: &AbiAttribute) -> Result<Option<AttributeValue>, DecodeError> {
    a.to_value()
        .map_err(|reason| DecodeError::InvalidAttributeValue {
            name: a.name.0,
            value_type: a.typeId,
            reason,
        })
}

fn attr_name(a: &AbiAttribute) -> Vec<u8> {
    strip_trailing_zeros(a.name.0.to_vec())
}

fn strip_trailing_zeros(mut v: Vec<u8>) -> Vec<u8> {
    while matches!(v.last(), Some(0)) {
        v.pop();
    }
    v
}

/// Why decoding `execute` calldata failed. These are structural faults; stateful
/// checks (ownership, expiry, transfer-to-self) are the executor's, not decode's.
#[derive(Debug)]
pub enum DecodeError {
    /// Fewer than four bytes — no room for a selector.
    CalldataTooShort,
    /// The leading selector isn't `execute(Operation[])`.
    UnknownSelector([u8; 4]),
    /// The `Operation[]` body didn't ABI-decode.
    Abi(String),
    /// `execute` was called with no operations.
    EmptyBatch,
    /// An operation's tag byte isn't one of the five known kinds.
    InvalidOpType(u8),
    /// An op's `operationData` didn't decode as its tag's struct, or wasn't
    /// canonically encoded.
    OperationData { operation: u8, reason: OpAbiError },
    /// A `patch` carried no mutations — it would be a no-op costing gas.
    EmptyMutations { key: EntityKey },
    /// Resolved expiry is at or before the current block.
    ExpiryDeadOnArrival {
        target: BlockNumber,
        current: BlockNumber,
    },
    /// `current + minLifetime` overflowed.
    ExpiryOverflow {
        current: BlockNumber,
        min_lifetime: u64,
    },
    /// A `create` set bits outside [`CREATION_FLAGS_MASK`].
    ReservedCreationFlags(u8),
    /// A `create` carried a tombstone; nothing exists yet to unset.
    TombstoneInCreate { name: Vec<u8> },
    /// A triple named a `$` attribute the engine owns.
    SystemAttributeNotWritable { name: Vec<u8> },
    /// A `Transfer` named the zero address as the new owner.
    TransferToZeroAddress { key: EntityKey },
    /// An attribute's value isn't a well-formed encoding of its declared type.
    InvalidAttributeValue {
        name: [u8; 32],
        value_type: u8,
        reason: AttrAbiError,
    },
    /// An op carried more attributes than the protocol allows.
    TooManyAttributes { count: usize, max: usize },
    /// Attributes aren't strictly ascending by name (also enforces uniqueness).
    AttributesNotSorted,
    /// An attribute name starts with a null byte — an empty identifier.
    AttributeNameEmpty,
    /// An attribute name byte is outside the `Ident32` charset for its position.
    AttributeNameInvalidByte { position: usize, value: u8 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = |n: &[u8]| String::from_utf8_lossy(n).into_owned();
        match self {
            DecodeError::CalldataTooShort => write!(f, "calldata too short for a selector"),
            DecodeError::UnknownSelector(s) => write!(f, "unknown selector {s:02x?}"),
            DecodeError::Abi(e) => write!(f, "invalid execute calldata: {e}"),
            DecodeError::EmptyBatch => write!(f, "execute called with an empty batch"),
            DecodeError::InvalidOpType(t) => write!(f, "invalid operation type {t}"),
            DecodeError::OperationData { operation, reason } => {
                write!(f, "operation {operation}: {reason}")
            }
            DecodeError::EmptyMutations { .. } => write!(f, "patch carried no mutations"),
            DecodeError::ExpiryDeadOnArrival { target, current } => write!(
                f,
                "resolved expiry {target} is not past the current block {current}"
            ),
            DecodeError::ExpiryOverflow {
                current,
                min_lifetime,
            } => write!(f, "expiry overflow ({current} + {min_lifetime})"),
            DecodeError::ReservedCreationFlags(b) => {
                write!(f, "creation flags set reserved bits (0b{b:08b})")
            }
            DecodeError::TombstoneInCreate { name } => {
                write!(f, "create carried a tombstone for '{}'", shown(name))
            }
            DecodeError::SystemAttributeNotWritable { name } => {
                write!(f, "'{}' is maintained by the engine", shown(name))
            }
            DecodeError::TransferToZeroAddress { .. } => write!(f, "transfer to the zero address"),
            DecodeError::InvalidAttributeValue {
                value_type, reason, ..
            } => write!(f, "invalid attribute value (type {value_type}): {reason}"),
            DecodeError::TooManyAttributes { count, max } => {
                write!(f, "too many attributes ({count} > {max})")
            }
            DecodeError::AttributesNotSorted => {
                write!(f, "attributes not sorted ascending by name")
            }
            DecodeError::AttributeNameEmpty => write!(f, "attribute name is empty"),
            DecodeError::AttributeNameInvalidByte { position, value } => write!(
                f,
                "attribute name has invalid byte 0x{value:02x} at position {position}"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes, FixedBytes};
    use arkiv_bindings::{AttributeType, Ident32};

    fn calldata(ops: Vec<Operation>) -> Vec<u8> {
        IEntityRegistry::executeCall { ops }.abi_encode()
    }

    fn env(caller: [u8; 20], block: u64, chain_id: u64) -> ExecEnv {
        ExecEnv {
            caller,
            block_number: block,
            gas_supplied: 1_000_000,
            chain_id,
        }
    }

    fn ident(name: &str) -> Ident32 {
        if name.starts_with('$') {
            Ident32::system(name).unwrap()
        } else {
            Ident32::encode(name).unwrap()
        }
    }

    fn attr(name: &str, value: &AttributeValue) -> AbiAttribute {
        AbiAttribute::from_value(ident(name), value).unwrap()
    }

    /// A relative-lifetime create: `expiresAt = 0`, so the floor decides.
    fn create_in(min_lifetime: u64, attrs: Vec<AbiAttribute>) -> Operation {
        Operation::create(0, 0, min_lifetime, 0, attrs)
    }

    // -------------------------------------------------------------------------
    // Create
    // -------------------------------------------------------------------------

    #[test]
    fn decodes_create_with_derived_key_and_resolved_expiry() {
        let cd = calldata(vec![create_in(50, vec![])]);
        let ops = decode_ops(&env([0xAA; 20], 10, 1), &cd, EntityNonce::new(7)).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            Op::Create {
                key, expires_at, ..
            } => {
                assert_eq!(
                    *key,
                    derive_entity_key(1, &[0xAA; 20], EntityNonce::new(7), 0)
                ); // nonce = start_nonce
                assert_eq!(*expires_at, 60); // block 10 + minLifetime 50
            }
            other => panic!("expected create, got {other:?}"),
        }
    }

    #[test]
    fn successive_creates_mint_sequential_keys() {
        let cd = calldata(vec![create_in(1, vec![]), create_in(1, vec![])]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(100)).unwrap();
        assert_eq!(
            *ops[0].key(),
            derive_entity_key(1, &[0xAA; 20], EntityNonce::new(100), 0)
        );
        assert_eq!(
            *ops[1].key(),
            derive_entity_key(1, &[0xAA; 20], EntityNonce::new(101), 0)
        );
    }

    /// The salt is what makes a key unpredictable, so it must change the key —
    /// otherwise it is decoration and the pre-image window stays open.
    #[test]
    fn salt_changes_the_minted_key() {
        let key_of = |salt: u128| {
            let cd = calldata(vec![Operation::create(salt, 0, 1, 0, vec![])]);
            *decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)).unwrap()[0].key()
        };
        assert_ne!(key_of(0), key_of(1));
        assert_ne!(key_of(1), key_of(u128::MAX));
        // Same salt, same everything: still deterministic.
        assert_eq!(key_of(42), key_of(42));
    }

    /// Key derivation is consensus-critical and mirrored in the SDK, so every
    /// input must actually reach the preimage.
    #[test]
    fn every_derivation_input_changes_the_key() {
        let base = derive_entity_key(1, &[0xAA; 20], EntityNonce::new(5), 9);
        assert_ne!(
            base,
            derive_entity_key(2, &[0xAA; 20], EntityNonce::new(5), 9),
            "chain_id"
        );
        assert_ne!(
            base,
            derive_entity_key(1, &[0xBB; 20], EntityNonce::new(5), 9),
            "owner"
        );
        assert_ne!(
            base,
            derive_entity_key(1, &[0xAA; 20], EntityNonce::new(6), 9),
            "nonce"
        );
        assert_ne!(
            base,
            derive_entity_key(1, &[0xAA; 20], EntityNonce::new(5), 8),
            "salt"
        );
    }

    #[test]
    fn create_lifts_payload_and_content_type_out_of_the_attributes() {
        let cd = calldata(vec![create_in(
            1,
            vec![
                attr("$contentType", &AttributeValue::Str("text/plain".into())),
                attr("$payload", &AttributeValue::Bytes(b"hi".to_vec())),
                attr("rank", &AttributeValue::u256_from_u64(3)),
            ],
        )]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)).unwrap();
        let Op::Create {
            content_type,
            payload,
            attributes,
            ..
        } = &ops[0]
        else {
            panic!("expected create");
        };
        assert_eq!(content_type, b"text/plain");
        assert_eq!(payload, b"hi");
        // Only the user attribute is left.
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].key, b"rank");
    }

    #[test]
    fn create_records_creation_flags() {
        let cd = calldata(vec![Operation::create(0, 0, 1, 0b11, vec![])]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)).unwrap();
        let Op::Create { creation_flags, .. } = &ops[0] else {
            panic!("expected create");
        };
        assert_eq!(creation_flags.bits(), 0b11);
    }

    /// Reserved bits must stay reserved — accepting them now would make them
    /// unusable later, since old nodes would have ignored whatever they meant.
    #[test]
    fn create_rejects_reserved_creation_flags() {
        for flags in [0b100, 0b1000_0000, 0xFF] {
            let cd = calldata(vec![Operation::create(0, 0, 1, flags, vec![])]);
            assert!(matches!(
                decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
                Err(DecodeError::ReservedCreationFlags(_)),
            ));
        }
    }

    #[test]
    fn create_rejects_a_tombstone() {
        let cd = calldata(vec![create_in(
            1,
            vec![AbiAttribute::tombstone(Ident32::encode("gone").unwrap())],
        )]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
            Err(DecodeError::TombstoneInCreate { .. })
        ));
    }

    // -------------------------------------------------------------------------
    // Expiry resolution
    // -------------------------------------------------------------------------

    /// The single `max` rule, mode by mode.
    #[test]
    fn expiry_resolves_absolute_relative_and_floor() {
        // Relative: expiresAt = 0, so the floor wins.
        assert_eq!(resolve_expiry(100, 0, 50).unwrap(), 150);
        // Absolute: a future expiresAt with no floor.
        assert_eq!(resolve_expiry(100, 500, 0).unwrap(), 500);
        // Absolute + floor: the floor raises a too-close target.
        assert_eq!(resolve_expiry(100, 110, 50).unwrap(), 150);
        // ...but does not lower a further one.
        assert_eq!(resolve_expiry(100, 500, 50).unwrap(), 500);
        // Permanent.
        assert_eq!(resolve_expiry(100, u64::MAX, 0).unwrap(), u64::MAX);
    }

    #[test]
    fn expiry_rejects_dead_on_arrival_and_overflow() {
        // Both args zero, and a past or current target.
        for (expires_at, min_lifetime) in [(0, 0), (100, 0), (50, 0)] {
            assert!(matches!(
                resolve_expiry(100, expires_at, min_lifetime),
                Err(DecodeError::ExpiryDeadOnArrival { .. })
            ));
        }
        // An absurd floor reverts rather than wrapping around to a live block.
        assert!(matches!(
            resolve_expiry(100, 0, u64::MAX),
            Err(DecodeError::ExpiryOverflow { .. })
        ));
    }

    // -------------------------------------------------------------------------
    // Patch
    // -------------------------------------------------------------------------

    #[test]
    fn decodes_patch_sets_and_unsets() {
        let key = B256::repeat_byte(9);
        let cd = calldata(vec![Operation::patch(
            key,
            vec![
                attr("color", &AttributeValue::Str("blue".into())),
                AbiAttribute::tombstone(Ident32::encode("size").unwrap()),
            ],
        )]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)).unwrap();
        let Op::Patch { key: k, mutations } = &ops[0] else {
            panic!("expected patch");
        };
        assert_eq!(*k, key.0);
        assert_eq!(mutations.len(), 2);
        assert_eq!(
            mutations[0],
            AttributeMutation::set(b"color".to_vec(), AttributeValue::Str("blue".into()))
        );
        assert_eq!(mutations[1], AttributeMutation::unset(b"size".to_vec()));
    }

    /// `$payload` / `$contentType` stay in the mutation list — a patch is a
    /// delta, so "not mentioned" and "set to empty" must stay distinguishable.
    #[test]
    fn patch_keeps_user_managed_system_attributes_as_mutations() {
        let cd = calldata(vec![Operation::patch(
            B256::repeat_byte(1),
            vec![attr("$payload", &AttributeValue::Bytes(b"new".to_vec()))],
        )]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)).unwrap();
        let Op::Patch { mutations, .. } = &ops[0] else {
            panic!("expected patch");
        };
        assert_eq!(mutations[0].key, b"$payload");
    }

    #[test]
    fn patch_rejects_an_empty_mutation_list() {
        let cd = calldata(vec![Operation::patch(B256::repeat_byte(1), vec![])]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
            Err(DecodeError::EmptyMutations { .. })
        ));
    }

    // -------------------------------------------------------------------------
    // System-attribute authority
    // -------------------------------------------------------------------------

    /// The engine owns these cells. Naming one — set *or* tombstone, create *or*
    /// patch — is a revert, so a client can never forge provenance or lifecycle.
    #[test]
    fn engine_controlled_system_attributes_are_rejected() {
        for name in [
            "$owner",
            "$creator",
            "$createdAtBlock",
            "$expiration",
            "$key",
        ] {
            let set =
                AbiAttribute::from_value(ident(name), &AttributeValue::u256_from_u64(1)).unwrap();
            for op in [
                Operation::create(0, 0, 1, 0, vec![set.clone()]),
                Operation::patch(B256::repeat_byte(1), vec![set.clone()]),
                Operation::patch(
                    B256::repeat_byte(1),
                    vec![AbiAttribute::tombstone(ident(name))],
                ),
            ] {
                assert!(
                    matches!(
                        decode_ops(
                            &env([0xAA; 20], 1, 1),
                            &calldata(vec![op]),
                            EntityNonce::ZERO
                        ),
                        Err(DecodeError::SystemAttributeNotWritable { .. })
                    ),
                    "{name} should be rejected"
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // The keyed ops
    // -------------------------------------------------------------------------

    #[test]
    fn maps_the_keyed_ops() {
        let key = B256::repeat_byte(9);
        let owner = Address::repeat_byte(0xBB);
        let cd = calldata(vec![
            Operation::extend_expiry(key, 0, 5),
            Operation::transfer_ownership(key, owner),
            Operation::delete(key),
        ]);
        let ops = decode_ops(&env([0xAA; 20], 100, 1), &cd, EntityNonce::new(0)).unwrap();
        assert!(
            matches!(&ops[0], Op::ExtendExpiry { new_expires_at, .. } if *new_expires_at == 105)
        );
        assert!(
            matches!(&ops[1], Op::Transfer { new_owner, .. } if *new_owner == owner.into_array())
        );
        assert!(matches!(&ops[2], Op::Delete { .. }));
    }

    #[test]
    fn rejects_transfer_to_zero() {
        let cd = calldata(vec![Operation::transfer_ownership(
            B256::repeat_byte(1),
            Address::ZERO,
        )]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
            Err(DecodeError::TransferToZeroAddress { .. })
        ));
    }

    // -------------------------------------------------------------------------
    // Structural faults
    // -------------------------------------------------------------------------

    #[test]
    fn rejects_empty_batch() {
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &calldata(vec![]), EntityNonce::ZERO),
            Err(DecodeError::EmptyBatch)
        ));
    }

    #[test]
    fn rejects_unknown_op_tag() {
        let cd = calldata(vec![Operation {
            operation: 99,
            operationData: Bytes::new(),
        }]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
            Err(DecodeError::InvalidOpType(99))
        ));
    }

    /// One op, one encoding: a blob with trailing junk decodes under a
    /// permissive reader but is not what the encoder emits.
    #[test]
    fn rejects_non_canonical_operation_data() {
        let mut op = Operation::delete(B256::repeat_byte(1));
        op.operationData = {
            let mut b = op.operationData.to_vec();
            b.extend_from_slice(&[0u8; 32]);
            Bytes::from(b)
        };
        assert!(matches!(
            decode_ops(
                &env([0xAA; 20], 1, 1),
                &calldata(vec![op]),
                EntityNonce::ZERO
            ),
            Err(DecodeError::OperationData {
                reason: OpAbiError::NonCanonical,
                ..
            })
        ));
    }

    #[test]
    fn rejects_unsorted_and_duplicate_attributes() {
        let a = attr("b", &AttributeValue::u256_from_u64(1));
        let b = attr("a", &AttributeValue::u256_from_u64(2));
        // Hand-built (the constructors sort), so the order is what reaches decode.
        let unsorted = Operation {
            operation: OP_PATCH,
            operationData: Bytes::from(alloy_sol_types::SolValue::abi_encode(&Patch {
                entityKey: B256::repeat_byte(1),
                mutations: vec![a.clone(), b],
            })),
        };
        assert!(matches!(
            decode_ops(
                &env([0xAA; 20], 1, 1),
                &calldata(vec![unsorted]),
                EntityNonce::ZERO
            ),
            Err(DecodeError::AttributesNotSorted)
        ));

        let duplicate = Operation {
            operation: OP_PATCH,
            operationData: Bytes::from(alloy_sol_types::SolValue::abi_encode(&Patch {
                entityKey: B256::repeat_byte(1),
                mutations: vec![a.clone(), a],
            })),
        };
        assert!(matches!(
            decode_ops(
                &env([0xAA; 20], 1, 1),
                &calldata(vec![duplicate]),
                EntityNonce::ZERO
            ),
            Err(DecodeError::AttributesNotSorted)
        ));
    }

    /// Attribute names must be valid `Ident32`s — an uppercase byte (as an SDK
    /// sends for `"testInvalidKey"`) reports its exact position and value.
    #[test]
    fn rejects_invalid_attribute_name() {
        let mut name = [0u8; 32];
        name[..14].copy_from_slice(b"testInvalidKey");
        let bad = AbiAttribute {
            name: FixedBytes::from(name),
            typeId: AttributeType::Str.id(),
            value: Bytes::new(),
        };
        let cd = calldata(vec![Operation::patch(B256::repeat_byte(1), vec![bad])]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
            // "testInvalidKey": the first bad byte is 'I' (0x49) at position 4.
            Err(DecodeError::AttributeNameInvalidByte {
                position: 4,
                value: 0x49,
            })
        ));
    }

    #[test]
    fn rejects_empty_attribute_name() {
        let bad = AbiAttribute {
            name: FixedBytes::from([0u8; 32]),
            typeId: AttributeType::Str.id(),
            value: Bytes::new(),
        };
        let cd = calldata(vec![Operation::patch(B256::repeat_byte(1), vec![bad])]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, EntityNonce::new(0)),
            Err(DecodeError::AttributeNameEmpty)
        ));
    }

    #[test]
    fn rejects_unknown_selector() {
        assert!(matches!(
            decode_ops(
                &env([0xAA; 20], 1, 1),
                &[0xDE, 0xAD, 0xBE, 0xEF, 0x00],
                EntityNonce::ZERO
            ),
            Err(DecodeError::UnknownSelector(_))
        ));
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &[0x01, 0x02], EntityNonce::ZERO),
            Err(DecodeError::CalldataTooShort)
        ));
    }
}
