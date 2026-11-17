//! Running transactions.

use alloc::vec::Vec;
use core::fmt;

use crate::entity::{Attribute, AttributeValue, CreationFlags};
use crate::gas::GasBreakdown;
use crate::primitives::{BlockNumber, EntityAddress, Gas, UserAddress};

/// What a transaction executor needs to know about its context. No EVM call
/// types — just these fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecEnv {
    /// Who signed the transaction.
    pub caller: UserAddress,
    /// The block being executed.
    pub block_number: BlockNumber,
    /// Gas available to this transaction.
    pub gas_supplied: Gas,
    /// The transaction's own terms: 21,000 plus its calldata. Added to compute
    /// once, before any operation.
    pub tx_gas: Gas,
    /// The least compute gas the transaction pays (EIP-7623), applied once,
    /// when the batch's gas is finished.
    pub floor_gas: Gas,
    /// GolemDB's work the transaction did before its batch, the read of the
    /// caller's minting nonce its entity keys are derived from. Entity cost
    /// like the batch's own, so the batch's meter starts from it.
    pub golemdb_cost_before: Gas,
    /// Chain id.
    pub chain_id: u64,
    /// The furthest ahead, in blocks, a create or extend may set an expiry:
    /// the fee schedule's `max_lifetime`, at the block's base.
    pub max_lifetime: BlockNumber,
    /// Whether a successful batch keeps its writes. `false` for a question
    /// rather than a transaction (`eth_call`, `eth_estimateGas`): its writes
    /// are discarded like a reverted batch's, so one probe never sees another.
    pub keep_writes: bool,
}

/// How an execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStatus {
    /// Succeeded; its changes stand.
    Ok,
    /// Failed; rolled back with no changes.
    Reverted,
}

/// The result of running a transaction's operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub status: ExecStatus,
    /// The gas charged, by what it pays for. Its total is `gas_used`.
    pub gas: GasBreakdown,
    /// Why it reverted, when `status` is [`ExecStatus::Reverted`].
    pub revert: Option<RevertReason>,
}

impl ExecOutput {
    /// The gas charged: compute, storage and purge together. A breakdown the
    /// executor returns always has a total.
    pub fn gas_used(&self) -> Gas {
        self.gas
            .total()
            .expect("the executor only returns a breakdown whose total fits")
    }
}

/// Why a batch reverted — a structured, host-agnostic reason.
///
/// The business logic reports *what* failed with the state it observed; how a
/// reason is presented on the wire (e.g. ABI-encoded Solidity errors) is the
/// host's concern. `Display` carries the human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertReason {
    /// A `Create` targeted a key that already exists.
    AlreadyExists { key: EntityAddress },
    /// The op targeted a key with no live entity.
    NotFound { key: EntityAddress },
    /// The caller isn't the entity's owner.
    NotOwner {
        key: EntityAddress,
        caller: UserAddress,
        owner: UserAddress,
    },
    /// A mutation targeted an entity past its expiry.
    Expired {
        key: EntityAddress,
        expires_at: BlockNumber,
    },
    /// A `Patch` targeted an entity created with the `readonly` flag.
    ReadOnly { key: EntityAddress },
    /// A `Patch` would leave the entity with too many user attributes.
    TooManyAttributes { count: usize, max: usize },
    /// An `ExtendExpiry` would have moved the expiry backwards.
    ExpiryNotExtended {
        key: EntityAddress,
        new_expires_at: BlockNumber,
        current_expires_at: BlockNumber,
    },
    /// A `Transfer` named the current owner as the new owner.
    TransferToSelf { key: EntityAddress },
    /// A `Create` or `ExtendExpiry` would leave more lifetime than the fee
    /// schedule's `max_lifetime`, or asked for permanent expiry, which is not
    /// sold.
    ExpiryBeyondMaxLifetime {
        target: BlockNumber,
        current_block: BlockNumber,
        max_lifetime: BlockNumber,
    },
    /// The batch's cost exceeded the gas supplied.
    OutOfGas,
}

impl fmt::Display for RevertReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
            write!(f, "0x")?;
            for b in bytes {
                write!(f, "{b:02x}")?;
            }
            Ok(())
        }
        match self {
            Self::AlreadyExists { key } => {
                write!(f, "entity ")?;
                hex(f, key)?;
                write!(f, " already exists")
            }
            Self::NotFound { key } => {
                write!(f, "entity ")?;
                hex(f, key)?;
                write!(f, " does not exist")
            }
            Self::NotOwner { key, .. } => {
                write!(f, "caller does not own entity ")?;
                hex(f, key)
            }
            Self::Expired { key, .. } => {
                write!(f, "entity ")?;
                hex(f, key)?;
                write!(f, " has expired")
            }
            Self::ReadOnly { key } => {
                write!(f, "entity ")?;
                hex(f, key)?;
                write!(f, " is read-only")
            }
            Self::TooManyAttributes { count, max } => {
                write!(f, "too many attributes ({count} > {max})")
            }
            Self::ExpiryNotExtended {
                key,
                new_expires_at,
                current_expires_at,
            } => {
                write!(f, "entity ")?;
                hex(f, key)?;
                write!(
                    f,
                    " expiry not extended ({new_expires_at} <= {current_expires_at})"
                )
            }
            Self::TransferToSelf { key } => {
                write!(f, "transfer of entity ")?;
                hex(f, key)?;
                write!(f, " to its current owner")
            }
            Self::ExpiryBeyondMaxLifetime {
                target,
                current_block,
                max_lifetime,
            } => write!(
                f,
                "expiry {target} at block {current_block} exceeds the maximum lifetime \
                 of {max_lifetime} blocks"
            ),
            Self::OutOfGas => write!(f, "out of gas"),
        }
    }
}

/// One operation on one entity.
///
/// A transaction carries a list of these; turning raw `op_bytes` into them is the
/// host's decoding step. `creator` and initial `owner` aren't here — they are the
/// caller ([`ExecEnv::caller`]), and ownership only moves via [`Op::Transfer`].
/// `expires_at` is an absolute block; any relative "blocks to live" is resolved
/// while decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Create {
        key: EntityAddress,
        expires_at: BlockNumber,
        /// Entity properties fixed at creation. Immutable thereafter.
        creation_flags: CreationFlags,
        attributes: Vec<Attribute>,
    },
    /// Partially mutate an entity: set some attributes, unset others, leave the
    /// rest alone.
    ///
    /// This is **not** a whole-entity replace. Two patches touching disjoint
    /// attributes compose instead of clobbering one another, and the cost is
    /// proportional to the mutation count rather than the entity size.
    Patch {
        key: EntityAddress,
        mutations: Vec<AttributeMutation>,
    },
    ExtendExpiry {
        key: EntityAddress,
        new_expires_at: BlockNumber,
    },
    Transfer {
        key: EntityAddress,
        new_owner: UserAddress,
    },
    Delete {
        key: EntityAddress,
    },
}

/// One entry in a [`Patch`](Op::Patch)'s mutation list: set an attribute, or
/// unset it.
///
/// `key` names a user attribute; every `$` name is rejected while decoding, so
/// the engine's authority over `$owner`, `$expiresAt` and friends is never in
/// question here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeMutation {
    pub key: Vec<u8>,
    /// The value to set, or `None` to unset the attribute — a *tombstone* on
    /// the wire (`typeId` 0).
    pub value: Option<AttributeValue>,
    /// Whether the set value is indexed. A set replaces the attribute whole, so
    /// this may flip an existing attribute either way. `false` on a tombstone.
    pub indexed: bool,
}

impl AttributeMutation {
    /// Set `key` to `value`, indexed.
    pub fn set(key: impl Into<Vec<u8>>, value: AttributeValue) -> Self {
        Self {
            key: key.into(),
            value: Some(value),
            indexed: true,
        }
    }

    /// Set `key` to `value`, unindexed.
    pub fn set_unindexed(key: impl Into<Vec<u8>>, value: AttributeValue) -> Self {
        Self {
            key: key.into(),
            value: Some(value),
            indexed: false,
        }
    }

    /// Unset `key`.
    pub fn unset(key: impl Into<Vec<u8>>) -> Self {
        Self {
            key: key.into(),
            value: None,
            indexed: false,
        }
    }
}

impl Op {
    /// The entity this operation targets (the key being created, for `Create`).
    pub fn key(&self) -> &EntityAddress {
        match self {
            Op::Create { key, .. }
            | Op::Patch { key, .. }
            | Op::ExtendExpiry { key, .. }
            | Op::Transfer { key, .. }
            | Op::Delete { key } => key,
        }
    }

    /// This operation's kind, without its data — handy for logging and pricing.
    pub fn kind(&self) -> OpKind {
        match self {
            Op::Create { .. } => OpKind::Create,
            Op::Patch { .. } => OpKind::Patch,
            Op::ExtendExpiry { .. } => OpKind::ExtendExpiry,
            Op::Transfer { .. } => OpKind::Transfer,
            Op::Delete { .. } => OpKind::Delete,
        }
    }
}

/// The kind of an [`Op`], without its data.
///
/// There is no `Expire`: an expired entity is removed by the protocol's
/// per-block purge, not by a client-submitted operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Create,
    Patch,
    ExtendExpiry,
    Transfer,
    Delete,
}
