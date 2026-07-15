//! Running transactions and blocks.

use alloc::vec::Vec;
use core::fmt;

use crate::entity::Attribute;
use crate::primitives::{Address, BlockNumber, EntityKey, Gas, Hash};
use crate::state::{BlockAuxiliaryStoreDelta, BlockEntityStoreDelta, EntityStore};

/// What a transaction executor needs to know about its context. No EVM call
/// types — just these fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecEnv {
    /// Who signed the transaction.
    pub caller: Address,
    /// The block being executed.
    pub block_number: BlockNumber,
    /// Gas available to this transaction.
    pub gas_supplied: Gas,
    /// Chain id.
    pub chain_id: u64,
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
    /// Gas charged — a placeholder until [`CostModel`](crate::gas::CostModel) is
    /// filled in.
    pub gas_used: Gas,
    /// Why it reverted, when `status` is [`ExecStatus::Reverted`].
    pub revert: Option<RevertReason>,
}

/// Why a batch reverted — a structured, host-agnostic reason.
///
/// The business logic reports *what* failed with the state it observed; how a
/// reason is presented on the wire (e.g. ABI-encoded Solidity errors) is the
/// host's concern. `Display` carries the human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertReason {
    /// A `Create` targeted a key that already exists.
    AlreadyExists { key: EntityKey },
    /// The op targeted a key with no live entity.
    NotFound { key: EntityKey },
    /// The caller isn't the entity's owner.
    NotOwner {
        key: EntityKey,
        caller: Address,
        owner: Address,
    },
    /// A mutation targeted an entity past its expiry.
    Expired {
        key: EntityKey,
        expires_at: BlockNumber,
    },
    /// An `Expire` targeted an entity still within its lifetime.
    NotExpired {
        key: EntityKey,
        expires_at: BlockNumber,
    },
    /// An `ExtendExpiry` did not move the expiry forward.
    ExpiryNotExtended {
        key: EntityKey,
        new_expires_at: BlockNumber,
        current_expires_at: BlockNumber,
    },
    /// A `Transfer` named the current owner as the new owner.
    TransferToSelf { key: EntityKey },
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
            Self::NotExpired { key, .. } => {
                write!(f, "entity ")?;
                hex(f, key)?;
                write!(f, " has not expired")
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
        key: EntityKey,
        expires_at: BlockNumber,
        content_type: Vec<u8>,
        payload: Vec<u8>,
        attributes: Vec<Attribute>,
    },
    Update {
        key: EntityKey,
        content_type: Vec<u8>,
        payload: Vec<u8>,
        attributes: Vec<Attribute>,
    },
    ExtendExpiry {
        key: EntityKey,
        new_expires_at: BlockNumber,
    },
    Transfer {
        key: EntityKey,
        new_owner: Address,
    },
    Delete {
        key: EntityKey,
    },
    Expire {
        key: EntityKey,
    },
}

impl Op {
    /// The entity this operation targets (the key being created, for `Create`).
    pub fn key(&self) -> &EntityKey {
        match self {
            Op::Create { key, .. }
            | Op::Update { key, .. }
            | Op::ExtendExpiry { key, .. }
            | Op::Transfer { key, .. }
            | Op::Delete { key }
            | Op::Expire { key } => key,
        }
    }

    /// This operation's kind, without its data — handy for logging and pricing.
    pub fn kind(&self) -> OpKind {
        match self {
            Op::Create { .. } => OpKind::Create,
            Op::Update { .. } => OpKind::Update,
            Op::ExtendExpiry { .. } => OpKind::ExtendExpiry,
            Op::Transfer { .. } => OpKind::Transfer,
            Op::Delete { .. } => OpKind::Delete,
            Op::Expire { .. } => OpKind::Expire,
        }
    }
}

/// The kind of an [`Op`], without its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Create,
    Update,
    ExtendExpiry,
    Transfer,
    Delete,
    Expire,
}

/// A block's changes-in-progress: what execution has staged so far, to be applied
/// to the stores when the block commits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockDraft {
    /// Entity changes staged so far.
    pub entities: BlockEntityStoreDelta,
    /// Index changes staged so far.
    pub auxiliary: BlockAuxiliaryStoreDelta,
}

/// Runs one transaction: decode its operations, check them, and stage the
/// resulting changes into the block's [`BlockDraft`].
///
/// Reads see the committed [`EntityStore`] plus whatever earlier transactions in
/// the same block already staged in `draft`. Nothing touches the stores until the
/// block commits.
///
/// A transaction is all-or-nothing: if any operation fails, `draft` is left
/// exactly as it was.
pub trait TransactionExecutor {
    /// The entity store this executor reads from.
    type Entities: EntityStore;
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// Run `op_bytes` under `env`, reading `entities` and `draft` and staging this
    /// transaction's changes into `draft`.
    fn execute(
        &self,
        env: &ExecEnv,
        entities: &mut Self::Entities,
        draft: &mut BlockDraft,
        op_bytes: &[u8],
    ) -> Result<ExecOutput, Self::Error>;
}

/// Runs a block of transactions and, on commit, produces its commitments. Also
/// undoes blocks on a reorg.
pub trait BlockExecutor {
    /// Error type — your choice; it only has to be `Debug`.
    type Error: core::fmt::Debug;

    /// Start a new block.
    fn begin_block(&mut self, block: BlockNumber) -> Result<(), Self::Error>;

    /// Run one transaction in the current block.
    fn execute_tx(&mut self, env: &ExecEnv, op_bytes: &[u8]) -> Result<ExecOutput, Self::Error>;

    /// Finish the block: apply its staged changes and return its commitments.
    fn commit_block(&mut self) -> Result<BlockCommit, Self::Error>;

    /// Undo every block from `block` onward (a reorg).
    fn rollback_to(&mut self, block: BlockNumber) -> Result<(), Self::Error>;
}

/// What committing a block produces. The entities and the index commit to
/// **separate** roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockCommit {
    /// The block just committed.
    pub block: BlockNumber,
    /// Root of the entities after this block.
    pub committed_root: Hash,
    /// Root of the index after this block.
    pub auxiliary_root: Hash,
    /// Running hash of all changes through this block.
    pub change_set_hash: Hash,
}
