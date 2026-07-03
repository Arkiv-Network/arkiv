//! ABI dispatch surface for `arkiv-db-engine`.
//!
//! Exposes a single [`dispatch`] entry point that `arkiv-executor` calls
//! with raw calldata bytes. All ABI knowledge (selectors, `Operation`
//! struct layout, validation, event encoding) lives here — the executor
//! never sees these types.
//!
//! Ported from `arkiv-op-reth/crates/arkiv-node/src/precompile.rs`.

use alloy_primitives::{Address, B256, Bytes, FixedBytes, Log, U256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, sol};
use eyre::Result;

use crate::{
    ARKIV_ADDRESS, ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute as EntityAttribute,
    StateAdapter,
};

// ─── ABI mirror of EntityRegistry.sol ────────────────────────────────

sol! {
    #[derive(Debug)]
    struct Mime128 {
        bytes32[4] data;
    }

    #[derive(Debug)]
    struct Attribute {
        bytes32 name;
        uint8 valueType;
        bytes32[4] value;
    }

    #[derive(Debug)]
    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        uint32 btl;
        address newOwner;
    }

    function execute(Operation[] ops) external;
    function nonces(address owner) external view returns (uint32);

    event EntityOperation(
        bytes32 indexed entityKey,
        uint8 indexed operationType,
        address indexed owner,
        uint32 expiresAt,
        bytes32 entityHash
    );

    error Ident32Empty();
    error Ident32InvalidByte(uint256 position, bytes1 value);
    error EmptyBatch();
    error InvalidOpType(uint8 operationType);
    error ZeroBtl();
    error EntityNotFound(bytes32 entityKey);
    error NotOwner(bytes32 entityKey, address caller, address owner);
    error EntityExpired(bytes32 entityKey, uint32 expiresAt);
    error ExpiryNotExtended(bytes32 entityKey, uint32 newExpiresAt, uint32 currentExpiresAt);
    error TransferToZeroAddress(bytes32 entityKey);
    error TransferToSelf(bytes32 entityKey);
    error EntityNotExpired(bytes32 entityKey, uint32 expiresAt);
    error AttributeValueMalformed(bytes32 name, uint8 valueType, uint256 wordIndex);
    error AttributeStringInvalidByte(bytes32 name, uint256 position, bytes1 value);
}

const OP_CREATE: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_EXTEND: u8 = 3;
const OP_TRANSFER: u8 = 4;
const OP_DELETE: u8 = 5;
const OP_EXPIRE: u8 = 6;

// ─── Public types ─────────────────────────────────────────────────────

/// Execution context the executor supplies; db-engine has no reth deps.
pub struct CallContext {
    pub caller: Address,
    pub chain_id: u64,
    pub block_number: u64,
}

/// Result of an ARKIV_ADDRESS call.
pub enum CallResult {
    /// ABI-encoded return bytes + logs to include in the receipt.
    Success { output: Bytes, logs: Vec<Log> },
    /// Solidity-style revert payload.
    Revert { data: Bytes },
}

/// Decode the first 4 bytes of `calldata` as a selector and route to
/// `nonces` or `execute`. Returns `Err` only on genuine internal failures
/// (state-DB errors); validation failures come back as `Ok(Revert {..})`.
pub fn dispatch<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    calldata: &[u8],
) -> Result<CallResult> {
    if calldata.len() < 4 {
        return Ok(CallResult::Revert {
            data: b"arkiv: calldata too short for selector".to_vec().into(),
        });
    }
    let selector: [u8; 4] = calldata[..4].try_into().unwrap();
    let body = &calldata[4..];

    match selector {
        noncesCall::SELECTOR => dispatch_nonces(state, body),
        executeCall::SELECTOR => dispatch_execute(state, ctx, body),
        _ => Ok(CallResult::Revert {
            data: format!(
                "arkiv: unknown selector 0x{}",
                alloy_primitives::hex::encode(selector)
            )
            .into_bytes()
            .into(),
        }),
    }
}

// ─── nonces(address) ─────────────────────────────────────────────────

fn dispatch_nonces<S: StateAdapter>(state: &mut S, body: &[u8]) -> Result<CallResult> {
    let decoded = match noncesCall::abi_decode_raw(body) {
        Ok(d) => d,
        Err(e) => {
            return Ok(CallResult::Revert {
                data: format!("arkiv: invalid nonces calldata: {e}")
                    .into_bytes()
                    .into(),
            });
        }
    };
    let nonce = crate::read_nonce(state, decoded.owner)?;
    let ret = noncesCall::abi_encode_returns(&nonce);
    Ok(CallResult::Success {
        output: ret.into(),
        logs: Vec::new(),
    })
}

// ─── execute(Operation[]) ─────────────────────────────────────────────

fn dispatch_execute<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    body: &[u8],
) -> Result<CallResult> {
    let ops = match executeCall::abi_decode_raw(body) {
        Ok(d) => d.ops,
        Err(e) => {
            return Ok(CallResult::Revert {
                data: format!("arkiv: invalid execute calldata: {e}")
                    .into_bytes()
                    .into(),
            });
        }
    };
    if ops.is_empty() {
        return Ok(CallResult::Revert {
            data: EmptyBatch {}.abi_encode().into(),
        });
    }

    let mut logs: Vec<Log> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        match apply_op(state, ctx, op, &mut logs) {
            Ok(()) => {}
            Err(ApplyError::Revert(payload)) => {
                tracing::debug!(op_index = i, "arkiv dispatch: op reverted");
                return Ok(CallResult::Revert { data: payload });
            }
            Err(ApplyError::Fatal(msg)) => {
                return Err(eyre::eyre!("arkiv dispatch fatal: {msg}"));
            }
        }
    }

    Ok(CallResult::Success {
        output: Bytes::new(),
        logs,
    })
}

// ─── Per-op application ───────────────────────────────────────────────

#[derive(Debug)]
enum ApplyError {
    Revert(Bytes),
    Fatal(String),
}

impl<E: std::fmt::Display> From<E> for ApplyError {
    fn from(err: E) -> Self {
        ApplyError::Fatal(err.to_string())
    }
}

fn apply_op<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    match op.operationType {
        OP_CREATE => apply_create(state, ctx, op, logs),
        OP_UPDATE => apply_update(state, ctx, op, logs),
        OP_EXTEND => apply_extend(state, ctx, op, logs),
        OP_TRANSFER => apply_transfer(state, ctx, op, logs),
        OP_DELETE => apply_delete(state, ctx, op, logs),
        OP_EXPIRE => apply_expire(state, ctx, op, logs),
        t => Err(ApplyError::Revert(
            InvalidOpType { operationType: t }.abi_encode().into(),
        )),
    }
}

fn apply_create<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    if op.btl == 0 {
        return Err(ApplyError::Revert(ZeroBtl {}.abi_encode().into()));
    }
    validate_attribute_names(&op.attributes)?;

    let expires_at = ctx.block_number.saturating_add(op.btl as u64);
    let attributes = convert_attributes(&op.attributes)?;
    let current_nonce = crate::bump_nonce(state, ctx.caller)
        .map_err(|e| ApplyError::Fatal(format!("bump nonce: {e}")))?;
    let entity_key = derive_entity_key(ctx.chain_id, ctx.caller, current_nonce);

    crate::create(
        state,
        ctx.caller,
        entity_key,
        expires_at,
        ctx.block_number,
        op.payload.to_vec(),
        mime128_to_bytes(&op.contentType),
        attributes,
    )?;

    emit_entity_op(logs, entity_key, OP_CREATE, ctx.caller, expires_at);
    Ok(())
}

fn apply_update<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    validate_attribute_names(&op.attributes)?;
    let entity = load_entity_for_owner(state, ctx.caller, ctx.block_number, op.entityKey, false)?;
    let attributes = convert_attributes(&op.attributes)?;
    crate::update(
        state,
        op.entityKey,
        ctx.block_number,
        op.payload.to_vec(),
        mime128_to_bytes(&op.contentType),
        attributes,
    )?;
    emit_entity_op(logs, op.entityKey, OP_UPDATE, entity.owner, entity.expires_at);
    Ok(())
}

fn apply_extend<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    if op.btl == 0 {
        return Err(ApplyError::Revert(ZeroBtl {}.abi_encode().into()));
    }
    let entity = load_entity_for_owner(state, ctx.caller, ctx.block_number, op.entityKey, false)?;
    let new_expires_at = ctx.block_number.saturating_add(op.btl as u64);
    if new_expires_at <= entity.expires_at {
        return Err(ApplyError::Revert(
            ExpiryNotExtended {
                entityKey: op.entityKey,
                newExpiresAt: clip_u32(new_expires_at),
                currentExpiresAt: clip_u32(entity.expires_at),
            }
            .abi_encode()
            .into(),
        ));
    }
    crate::extend(state, op.entityKey, ctx.block_number, new_expires_at)?;
    emit_entity_op(logs, op.entityKey, OP_EXTEND, entity.owner, new_expires_at);
    Ok(())
}

fn apply_transfer<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    let entity = load_entity_for_owner(state, ctx.caller, ctx.block_number, op.entityKey, false)?;
    if op.newOwner == Address::ZERO {
        return Err(ApplyError::Revert(
            TransferToZeroAddress { entityKey: op.entityKey }.abi_encode().into(),
        ));
    }
    if op.newOwner == entity.owner {
        return Err(ApplyError::Revert(
            TransferToSelf { entityKey: op.entityKey }.abi_encode().into(),
        ));
    }
    crate::transfer(state, op.entityKey, ctx.block_number, op.newOwner)?;
    emit_entity_op(logs, op.entityKey, OP_TRANSFER, op.newOwner, entity.expires_at);
    Ok(())
}

fn apply_delete<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    let entity = load_entity_for_owner(state, ctx.caller, ctx.block_number, op.entityKey, false)?;
    crate::delete(state, op.entityKey)?;
    emit_entity_op(logs, op.entityKey, OP_DELETE, entity.owner, entity.expires_at);
    Ok(())
}

fn apply_expire<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,
    op: &Operation,
    logs: &mut Vec<Log>,
) -> Result<(), ApplyError> {
    let entity = load_entity(state, op.entityKey)?
        .ok_or_else(|| not_found_revert(op.entityKey))?;
    if entity.expires_at > ctx.block_number {
        return Err(ApplyError::Revert(
            EntityNotExpired {
                entityKey: op.entityKey,
                expiresAt: clip_u32(entity.expires_at),
            }
            .abi_encode()
            .into(),
        ));
    }
    crate::expire(state, op.entityKey)?;
    emit_entity_op(logs, op.entityKey, OP_EXPIRE, entity.owner, entity.expires_at);
    Ok(())
}

// ─── Entity lookup + ownership guards ────────────────────────────────

struct ExistingEntity {
    owner: Address,
    expires_at: u64,
}

fn load_entity<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
) -> Result<Option<ExistingEntity>, ApplyError> {
    let entity_addr = crate::entity_address(entity_key);
    let code = state.code(&entity_addr)?;
    if code.is_empty() {
        return Ok(None);
    }
    let rlp = crate::EntityRlp::decode_from_code(&code)
        .map_err(|e| ApplyError::Fatal(format!("decode entity {entity_addr}: {e}")))?;
    Ok(Some(ExistingEntity { owner: rlp.owner, expires_at: rlp.expires_at }))
}

fn load_entity_for_owner<S: StateAdapter>(
    state: &mut S,
    caller: Address,
    current_block: u64,
    entity_key: B256,
    allow_expired: bool,
) -> Result<ExistingEntity, ApplyError> {
    let entity = load_entity(state, entity_key)?.ok_or_else(|| not_found_revert(entity_key))?;
    if !allow_expired && entity.expires_at <= current_block {
        return Err(ApplyError::Revert(
            EntityExpired {
                entityKey: entity_key,
                expiresAt: clip_u32(entity.expires_at),
            }
            .abi_encode()
            .into(),
        ));
    }
    if entity.owner != caller {
        return Err(ApplyError::Revert(
            NotOwner {
                entityKey: entity_key,
                caller,
                owner: entity.owner,
            }
            .abi_encode()
            .into(),
        ));
    }
    Ok(entity)
}

fn not_found_revert(entity_key: B256) -> ApplyError {
    ApplyError::Revert(EntityNotFound { entityKey: entity_key }.abi_encode().into())
}

// ─── Validation helpers ───────────────────────────────────────────────

const IDENT_CHARSET: u128 = {
    let mut m: u128 = 0;
    m |= 1u128 << 0x2D; // '-'
    m |= 1u128 << 0x2E; // '.'
    let mut b = 0x30u8;
    while b <= 0x39 {
        m |= 1u128 << b;
        b += 1;
    }
    m |= 1u128 << 0x5F; // '_'
    let mut b = 0x61u8;
    while b <= 0x7A {
        m |= 1u128 << b;
        b += 1;
    }
    m
};

const IDENT_LEADING: u128 = {
    let mut m: u128 = 0;
    let mut b = 0x61u8;
    while b <= 0x7A {
        m |= 1u128 << b;
        b += 1;
    }
    m
};

fn validate_ident32(raw: B256) -> Result<(), ApplyError> {
    let bytes = raw.0;
    if bytes[0] == 0 {
        return Err(ApplyError::Revert(Ident32Empty {}.abi_encode().into()));
    }
    let mut seen_zero = false;
    for (position, &b) in bytes.iter().enumerate() {
        if b == 0 {
            seen_zero = true;
        } else {
            let charset = if position == 0 { IDENT_LEADING } else { IDENT_CHARSET };
            let charset_bad = (b as u128) > 127 || (charset >> b) & 1 == 0;
            if seen_zero || charset_bad {
                return Err(ApplyError::Revert(
                    Ident32InvalidByte {
                        position: U256::from(position),
                        value: FixedBytes::<1>::from([b]),
                    }
                    .abi_encode()
                    .into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_attribute_names(attrs: &[Attribute]) -> Result<(), ApplyError> {
    for a in attrs {
        validate_ident32(a.name)?;
    }
    Ok(())
}

// ─── Encoding helpers ─────────────────────────────────────────────────

fn pack_bytes32_4(words: &[FixedBytes<32>; 4]) -> Vec<u8> {
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

fn mime128_to_bytes(m: &Mime128) -> Vec<u8> {
    pack_bytes32_4(&m.data)
}

fn ident32_to_bytes(name: B256) -> Vec<u8> {
    strip_trailing_zeros(name.0.to_vec())
}

fn reject_embedded_null_in_string(a: &Attribute) -> Result<(), ApplyError> {
    let mut seen_zero = false;
    for (position, b) in a
        .value
        .iter()
        .flat_map(|w| w.as_slice().iter())
        .enumerate()
    {
        if *b == 0 {
            seen_zero = true;
        } else if seen_zero {
            return Err(ApplyError::Revert(
                AttributeStringInvalidByte {
                    name: a.name,
                    position: U256::from(position),
                    value: FixedBytes::<1>::from([*b]),
                }
                .abi_encode()
                .into(),
            ));
        }
    }
    Ok(())
}

fn reject_invalid_utf8_in_string(a: &Attribute) -> Result<(), ApplyError> {
    let content = pack_bytes32_4(&a.value);
    if let Err(e) = std::str::from_utf8(&content) {
        let position = e.valid_up_to();
        return Err(ApplyError::Revert(
            AttributeStringInvalidByte {
                name: a.name,
                position: U256::from(position),
                value: FixedBytes::<1>::from([content[position]]),
            }
            .abi_encode()
            .into(),
        ));
    }
    Ok(())
}

fn reject_non_zero_upper_words(a: &Attribute) -> Result<(), ApplyError> {
    for (i, word) in a.value.iter().enumerate().skip(1) {
        if *word != FixedBytes::ZERO {
            return Err(ApplyError::Revert(
                AttributeValueMalformed {
                    name: a.name,
                    valueType: a.valueType,
                    wordIndex: U256::from(i),
                }
                .abi_encode()
                .into(),
            ));
        }
    }
    Ok(())
}

fn convert_attributes(attrs: &[Attribute]) -> Result<Vec<EntityAttribute>, ApplyError> {
    attrs
        .iter()
        .map(|a| {
            let key = ident32_to_bytes(a.name);
            let value = match a.valueType {
                ATTR_UINT => {
                    reject_non_zero_upper_words(a)?;
                    a.value[0].as_slice().to_vec()
                }
                ATTR_STRING => {
                    reject_embedded_null_in_string(a)?;
                    reject_invalid_utf8_in_string(a)?;
                    pack_bytes32_4(&a.value)
                }
                ATTR_ENTITY_KEY => {
                    reject_non_zero_upper_words(a)?;
                    a.value[0].as_slice().to_vec()
                }
                t => {
                    return Err(ApplyError::Fatal(format!(
                        "unknown attribute valueType {t}"
                    )));
                }
            };
            Ok(EntityAttribute {
                key,
                value_type: a.valueType,
                value,
            })
        })
        .collect()
}

// ─── Entity-key derivation ────────────────────────────────────────────

/// `keccak256(abi.encodePacked(chainId_be32, ARKIV_ADDRESS, owner, nonce_be4))`
/// Matches the SDK's local key derivation formula.
fn derive_entity_key(chain_id: u64, owner: Address, nonce: u32) -> B256 {
    let mut buf = Vec::with_capacity(32 + 20 + 20 + 4);
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(ARKIV_ADDRESS.as_slice());
    buf.extend_from_slice(owner.as_slice());
    buf.extend_from_slice(&nonce.to_be_bytes());
    keccak256(&buf)
}

// ─── Event emission ───────────────────────────────────────────────────

fn emit_entity_op(
    logs: &mut Vec<Log>,
    entity_key: B256,
    op_type: u8,
    owner: Address,
    expires_at: u64,
) {
    let event = EntityOperation {
        entityKey: entity_key,
        operationType: op_type,
        owner,
        expiresAt: clip_u32(expires_at),
        entityHash: B256::ZERO,
    };
    logs.push(Log {
        address: ARKIV_ADDRESS,
        data: event.encode_log_data(),
    });
}

fn clip_u32(n: u64) -> u32 {
    n.min(u32::MAX as u64) as u32
}
