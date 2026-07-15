//! ABI op decoding — the host's calldata → [`Op`] step.
//!
//! A client `CALL`s the Arkiv address with `execute(Operation[])` calldata (the
//! `IEntityRegistry` ABI, from `arkiv-bindings`). [`decode_ops`] turns those raw
//! bytes into the spec's host-agnostic [`Op`]s that [`ArkivExecutor::apply`] runs.
//! All ABI knowledge lives here; the executor never sees Solidity types.
//!
//! Two things are *resolved* during decode, per the [`Op`] contract:
//! - **`btl` → `expires_at`.** The ABI carries a relative "blocks to live"; the
//!   [`Op`] carries an absolute expiry (`env.block_number + btl`).
//! - **The create key.** The contract sends `entityKey = 0` for a `Create` and
//!   expects the node to mint the key from `(chain_id, caller, nonce)` — see
//!   [`derive_entity_key`]. A batch shares one caller, so successive creates use
//!   `start_nonce`, `start_nonce + 1`, …; reading and advancing the caller's
//!   persistent nonce is the wiring step's job, which is why `start_nonce` is a
//!   parameter and decode stays a pure function.
//!
//! [`ArkivExecutor::apply`]: crate::ArkivExecutor

use core::fmt;

use alloy_primitives::{Address, FixedBytes, U256, keccak256};
use alloy_sol_types::SolCall;
use arkiv_bindings::{
    ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute as AbiAttribute, IEntityRegistry,
    MAX_ATTRIBUTES, Mime128, OP_CREATE, OP_DELETE, OP_EXPIRE, OP_EXTEND, OP_TRANSFER, OP_UPDATE,
    types::{Ident32ByteError, validate_ident32_bytes},
};
use arkiv_interfaces::entity::Attribute;
use arkiv_interfaces::execution::{ExecEnv, Op};
use arkiv_interfaces::primitives::EntityKey;

use crate::ARKIV_ADDRESS;

/// Decode `execute(Operation[])` calldata into the batch's [`Op`]s.
///
/// `start_nonce` is the caller's entity-key minting nonce at the start of the
/// batch; the `i`-th `Create` in the batch mints its key from `start_nonce + i`.
pub fn decode_ops(
    env: &ExecEnv,
    calldata: &[u8],
    start_nonce: u32,
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
    let mut create_index: u32 = 0;
    for op in &batch.ops {
        let key: EntityKey = op.entityKey.0; // supplied by the client for non-creates
        let decoded = match op.operationType {
            OP_CREATE => {
                if op.btl == 0 {
                    return Err(DecodeError::ZeroBtl);
                }
                let nonce = start_nonce.saturating_add(create_index);
                create_index += 1;
                Op::Create {
                    key: derive_entity_key(env.chain_id, &env.caller, nonce),
                    expires_at: env.block_number.saturating_add(op.btl as u64),
                    content_type: mime128_to_bytes(&op.contentType),
                    payload: op.payload.to_vec(),
                    attributes: convert_attributes(&op.attributes)?,
                }
            }
            OP_UPDATE => Op::Update {
                key,
                content_type: mime128_to_bytes(&op.contentType),
                payload: op.payload.to_vec(),
                attributes: convert_attributes(&op.attributes)?,
            },
            OP_EXTEND => {
                if op.btl == 0 {
                    return Err(DecodeError::ZeroBtl);
                }
                Op::ExtendExpiry {
                    key,
                    new_expires_at: env.block_number.saturating_add(op.btl as u64),
                }
            }
            OP_TRANSFER => {
                if op.newOwner == Address::ZERO {
                    return Err(DecodeError::TransferToZeroAddress { key });
                }
                Op::Transfer {
                    key,
                    new_owner: op.newOwner.into_array(),
                }
            }
            OP_DELETE => Op::Delete { key },
            OP_EXPIRE => Op::Expire { key },
            other => return Err(DecodeError::InvalidOpType(other)),
        };
        out.push(decoded);
    }
    Ok(out)
}

/// Mint a `Create`'s entity key: `keccak256(chain_id_be32 || ARKIV_ADDRESS ||
/// owner || nonce_be4)`. Matches the SDK's local derivation, so a client can
/// predict the key it's about to create.
pub fn derive_entity_key(chain_id: u64, owner: &[u8; 20], nonce: u32) -> EntityKey {
    let mut buf = Vec::with_capacity(32 + 20 + 20 + 4);
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(ARKIV_ADDRESS.as_slice());
    buf.extend_from_slice(owner.as_slice());
    buf.extend_from_slice(&nonce.to_be_bytes());
    keccak256(&buf).0
}

/// Convert ABI attributes to entity attributes, decoding each value by its type.
///
/// Structural rules from the contract: at most [`MAX_ATTRIBUTES`], names are
/// valid `Ident32`s, and strictly ascending by name (which also enforces name
/// uniqueness).
fn convert_attributes(attrs: &[AbiAttribute]) -> Result<Vec<Attribute>, DecodeError> {
    if attrs.len() > MAX_ATTRIBUTES {
        return Err(DecodeError::TooManyAttributes {
            count: attrs.len(),
            max: MAX_ATTRIBUTES,
        });
    }
    for a in attrs {
        if let Err(e) = validate_ident32_bytes(&a.name.0) {
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
    attrs
        .iter()
        .map(|a| {
            let value = match a.valueType {
                // A 256-bit uint / an entity key each occupy exactly one word; the
                // upper three words must be zero.
                ATTR_UINT | ATTR_ENTITY_KEY => {
                    reject_non_zero_upper_words(a)?;
                    a.value[0].as_slice().to_vec()
                }
                // A string spans up to all four words; trailing zero padding is not
                // part of the value.
                ATTR_STRING => pack_words(&a.value),
                other => {
                    return Err(DecodeError::UnknownAttributeType {
                        name: a.name.0,
                        value_type: other,
                    });
                }
            };
            Ok(Attribute {
                key: strip_trailing_zeros(a.name.0.to_vec()),
                value_type: a.valueType,
                value,
            })
        })
        .collect()
}

/// Reject an attribute whose value claims one word but carries data in the others.
fn reject_non_zero_upper_words(a: &AbiAttribute) -> Result<(), DecodeError> {
    for (word_index, word) in a.value.iter().enumerate().skip(1) {
        if *word != FixedBytes::ZERO {
            return Err(DecodeError::AttributeValueMalformed {
                value_type: a.valueType,
                word_index,
            });
        }
    }
    Ok(())
}

/// The four 32-byte words concatenated, with trailing zero padding removed.
fn mime128_to_bytes(m: &Mime128) -> Vec<u8> {
    pack_words(&m.data)
}

fn pack_words(words: &[FixedBytes<32>; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    for w in words {
        out.extend_from_slice(w.as_slice());
    }
    strip_trailing_zeros(out)
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
    /// An operation's type byte isn't one of the six known kinds.
    InvalidOpType(u8),
    /// A `Create` or `Extend` gave a zero blocks-to-live.
    ZeroBtl,
    /// A `Transfer` named the zero address as the new owner.
    TransferToZeroAddress { key: EntityKey },
    /// A single-word attribute value carried data past its first word.
    AttributeValueMalformed { value_type: u8, word_index: usize },
    /// An attribute's value type isn't one of the known tags.
    UnknownAttributeType { name: [u8; 32], value_type: u8 },
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
        match self {
            DecodeError::CalldataTooShort => write!(f, "calldata too short for a selector"),
            DecodeError::UnknownSelector(s) => write!(f, "unknown selector {s:02x?}"),
            DecodeError::Abi(e) => write!(f, "invalid execute calldata: {e}"),
            DecodeError::EmptyBatch => write!(f, "execute called with an empty batch"),
            DecodeError::InvalidOpType(t) => write!(f, "invalid operation type {t}"),
            DecodeError::ZeroBtl => write!(f, "operation gave a zero blocks-to-live"),
            DecodeError::TransferToZeroAddress { .. } => write!(f, "transfer to the zero address"),
            DecodeError::AttributeValueMalformed {
                value_type,
                word_index,
            } => write!(
                f,
                "attribute value (type {value_type}) has data past word 0 (word {word_index})"
            ),
            DecodeError::UnknownAttributeType { value_type, .. } => {
                write!(f, "unknown attribute value type {value_type}")
            }
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
    use alloy_primitives::{B256, Bytes};
    use arkiv_bindings::{Attribute as AbiAttribute, Ident32, Operation};

    fn empty_mime() -> Mime128 {
        Mime128 {
            data: [FixedBytes::ZERO; 4],
        }
    }

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

    #[test]
    fn decodes_create_with_derived_key_and_resolved_expiry() {
        let cd = calldata(vec![Operation::create(
            50,
            Bytes::from_static(b"hi"),
            empty_mime(),
            vec![],
        )]);
        let ops = decode_ops(&env([0xAA; 20], 10, 1), &cd, 7).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            Op::Create {
                key,
                expires_at,
                payload,
                ..
            } => {
                assert_eq!(*key, derive_entity_key(1, &[0xAA; 20], 7)); // nonce = start_nonce
                assert_eq!(*expires_at, 60); // block 10 + btl 50
                assert_eq!(payload, b"hi");
            }
            other => panic!("expected create, got {other:?}"),
        }
    }

    #[test]
    fn successive_creates_mint_sequential_keys() {
        let cd = calldata(vec![
            Operation::create(1, Bytes::new(), empty_mime(), vec![]),
            Operation::create(1, Bytes::new(), empty_mime(), vec![]),
        ]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, 100).unwrap();
        assert_eq!(*ops[0].key(), derive_entity_key(1, &[0xAA; 20], 100));
        assert_eq!(*ops[1].key(), derive_entity_key(1, &[0xAA; 20], 101));
    }

    #[test]
    fn decodes_a_uint_attribute() {
        let attr = AbiAttribute::uint(Ident32::encode("age").unwrap(), U256::from(42));
        let cd = calldata(vec![Operation::create(
            1,
            Bytes::new(),
            empty_mime(),
            vec![attr],
        )]);
        let ops = decode_ops(&env([0xAA; 20], 1, 1), &cd, 0).unwrap();
        let Op::Create { attributes, .. } = &ops[0] else {
            panic!("expected create");
        };
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].key, b"age");
        assert_eq!(attributes[0].value_type, ATTR_UINT);
        assert_eq!(attributes[0].value, U256::from(42).to_be_bytes::<32>());
    }

    #[test]
    fn maps_the_keyed_ops() {
        let key = B256::repeat_byte(9);
        let owner = Address::repeat_byte(0xBB);
        let cd = calldata(vec![
            Operation::update(key, Bytes::from_static(b"p"), empty_mime(), vec![]),
            Operation::extend(key, 5),
            Operation::transfer(key, owner),
            Operation::delete(key),
            Operation::expire(key),
        ]);
        let ops = decode_ops(&env([0xAA; 20], 100, 1), &cd, 0).unwrap();
        assert!(matches!(&ops[0], Op::Update { key: k, .. } if *k == key.0));
        assert!(
            matches!(&ops[1], Op::ExtendExpiry { new_expires_at, .. } if *new_expires_at == 105)
        );
        assert!(
            matches!(&ops[2], Op::Transfer { new_owner, .. } if *new_owner == owner.into_array())
        );
        assert!(matches!(&ops[3], Op::Delete { .. }));
        assert!(matches!(&ops[4], Op::Expire { .. }));
    }

    #[test]
    fn rejects_empty_batch() {
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &calldata(vec![]), 0),
            Err(DecodeError::EmptyBatch)
        ));
    }

    #[test]
    fn rejects_zero_btl_create() {
        let cd = calldata(vec![Operation::create(
            0,
            Bytes::new(),
            empty_mime(),
            vec![],
        )]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, 0),
            Err(DecodeError::ZeroBtl)
        ));
    }

    /// Attribute names must be valid `Ident32`s — an uppercase byte (as an SDK
    /// sends for `"testInvalidKey"`) reports its exact position and value.
    #[test]
    fn rejects_invalid_attribute_name() {
        let mut name = [0u8; 32];
        name[..14].copy_from_slice(b"testInvalidKey");
        let attr = AbiAttribute {
            name: FixedBytes::from(name).into(),
            valueType: ATTR_STRING,
            value: [FixedBytes::ZERO; 4],
        };
        let cd = calldata(vec![Operation::create(
            10,
            Bytes::new(),
            empty_mime(),
            vec![attr],
        )]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, 0),
            // "testInvalidKey": the first bad byte is 'I' (0x49) at position 4.
            Err(DecodeError::AttributeNameInvalidByte {
                position: 4,
                value: 0x49,
            })
        ));
    }

    #[test]
    fn rejects_empty_attribute_name() {
        let attr = AbiAttribute {
            name: FixedBytes::from([0u8; 32]).into(),
            valueType: ATTR_STRING,
            value: [FixedBytes::ZERO; 4],
        };
        let cd = calldata(vec![Operation::create(
            10,
            Bytes::new(),
            empty_mime(),
            vec![attr],
        )]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, 0),
            Err(DecodeError::AttributeNameEmpty)
        ));
    }

    #[test]
    fn rejects_transfer_to_zero() {
        let cd = calldata(vec![Operation::transfer(
            B256::repeat_byte(1),
            Address::ZERO,
        )]);
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &cd, 0),
            Err(DecodeError::TransferToZeroAddress { .. })
        ));
    }

    #[test]
    fn rejects_unknown_selector() {
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &[0xDE, 0xAD, 0xBE, 0xEF, 0x00], 0),
            Err(DecodeError::UnknownSelector(_))
        ));
        assert!(matches!(
            decode_ops(&env([0xAA; 20], 1, 1), &[0x01, 0x02], 0),
            Err(DecodeError::CalldataTooShort)
        ));
    }
}
