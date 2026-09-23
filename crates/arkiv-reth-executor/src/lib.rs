//! Arkiv execution seam for reth — **no-EVM** executor.
//!
//! This crate owns the one component arkiv-reth overrides on top of reth's host:
//! the **executor** (`evm_config`). reth injects it via
//! `EthereumNode::components().executor(ArkivExecutorBuilder)` — no fork of reth.
//!
//! ## What this is
//!
//! The transaction interpreter is **bypassed**. We keep reth's stock
//! [`EthEvmConfig`] / `EthBlockExecutor` for block assembly, receipts, gas
//! accounting and state-root, but we plug in a custom [`ArkivEvm`] whose
//! [`transact_raw`](alloy_evm::Evm::transact_raw) applies a fixed-function state
//! transition **directly** — it never builds or runs the revm bytecode
//! interpreter. So for user/entity transactions, `revm`'s EVM is not invoked.
//!
//! Read that literally: it is not only the interpreter that is skipped but revm's
//! **whole handler pipeline**, and that includes its pre-execution *validation* stage.
//! Every check revm would have made — nonce, balance, base fee, chain id, EIP-3607 — is
//! therefore ours to make or to knowingly go without. Anything [`arkiv_transact`] is
//! handed and does not explicitly reject *succeeds*. The nonce is checked in
//! [`validate_nonce`] and the fee preconditions (fee cap vs. base fee, priority fee
//! vs. fee cap, balance vs. the maximum spend) in [`validate_fees`]; chain id and
//! EIP-3607 are not yet, which is a real gap and not a design choice.
//!
//! Fees follow EIP-1559 exactly as revm applies it ([`charge_sender`]): the sender
//! pays `gas_used × effective_gas_price`, where the effective price is
//! `min(max_fee, base_fee + priority_fee)` (a legacy tx's `gasPrice`); the base-fee
//! share is burned and the tip is credited to the block beneficiary. Unused gas is
//! never charged.
//!
//! - **Plain transfer** — debit sender (value + flat 21k gas), bump nonce,
//!   credit recipient. Computed in Rust, committed as a `BundleState`.
//! - **Call to [`ARKIV_ADDRESS`]** — the entity engine. The `execute(Operation[])`
//!   calldata is decoded to entity [`Op`]s and applied by [`ArkivExecutor`] over a
//!   reth-backed entity store, returning the account diff reth commits (plus the
//!   sender's gas charge) — see [`arkiv_transact`]. The one view function,
//!   `entityNonce(address)`, is answered directly from the minting-nonce slot —
//!   see [`arkiv_entity_nonce_call`].
//! - **Contract creation** — rejected (neutered): user programs never execute.
//!   The rejection is a *typed* invalid-tx error, so the payload builder skips a
//!   pooled deploy tx instead of aborting the build over it (the pool accepts
//!   creates and never evicts an unmined tx — a fatal error there would stall
//!   block production forever).
//!
//! ## Honest scope
//!
//! Two things still touch revm and are intentionally out of scope for this step:
//! the precompile set is the stock one (unused, since the interpreter never
//! runs), and **system-contract calls** (EIP-4788 beacon root / EIP-2935 block
//! hashes) in pre-execution still delegate to revm — they are protocol
//! housekeeping, not user execution. Both can be neutered later.
//!
//! The state boundary is fixed by reth regardless: we read through revm's
//! `Database` and write a `BundleState` in Ethereum's account model, committed
//! by the keccak-MPT (see the report, §3 and Appendix A): any execution *logic*,
//! not any state engine.
//!
//! ## Where the business logic lives
//!
//! This file is the **exact executor** — the reth-specific wiring (the [`Evm`],
//! [`EvmFactory`] and [`ExecutorBuilder`] reth injects). The **entity business
//! logic** is not here: it lives in [`arkiv`], written against the host-agnostic
//! [`arkiv_interfaces::execution::TransactionExecutor`] interface. And the
//! **state machinery** is not here either: all state — entities, the query
//! index, minting nonces, sender balances and EOA nonces — is reached through
//! one [`write_manager`] view (arkiv-reth-statemanager's `MptStateView` over
//! its `WriteOverlay`), so a transaction's every effect lands in a single
//! `EvmState` diff for reth to commit.

/// Entity business logic, implementing the `arkiv-interfaces` executor interface.
pub mod arkiv;
/// ABI op decoding: `execute(Operation[])` calldata → the spec's `Op`s.
pub mod decode;
/// Revert-payload encoding: `RevertReason` / `DecodeError` → Solidity error data.
pub mod revert;

pub use arkiv::{ArkivExecutor, OpEffect};
pub use decode::{DecodeError, decode_ops, derive_entity_address};

use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_evm::{Evm, EvmFactory, eth::EthEvmContext, precompiles::PrecompilesMap};
use alloy_primitives::{Address, B256, Bytes, Log, TxKind, U256};
use alloy_sol_types::{SolCall, SolError, SolEvent};
use arkiv_bindings::{IEntityRegistry, protocol::purgeExpiredCall};
use reth_ethereum::{
    EthPrimitives,
    chainspec::{EthereumHardforks, Hardforks},
    evm::{
        EthEvm, EthEvmConfig,
        primitives::{Database, EvmEnv},
        revm::{
            MainBuilder, MainContext,
            context::{BlockEnv, CfgEnv, Context, TxEnv},
            context_interface::{
                Cfg, Transaction,
                result::{
                    EVMError, ExecutionResult, HaltReason, InvalidTransaction, OutOfGasError,
                    Output, ResultAndState, ResultGas, SuccessReason,
                },
            },
            database_interface::DBErrorMarker,
            inspector::{Inspector, NoOpInspector},
            interpreter::interpreter::EthInterpreter,
            precompile::Precompiles,
            primitives::hardfork::SpecId,
            state::EvmState,
        },
    },
    node::{
        api::{FullNodeTypes, NodeTypes},
        builder::{BuilderContext, components::ExecutorBuilder},
    },
};

use core::cmp::Ordering;

use arkiv_interfaces::execution::{ExecEnv, ExecStatus, Op, OpKind};
use arkiv_interfaces::primitives::{Hash, UserBalance};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, AccountNoncesStore, BlockRef, EntityCreationNoncesStore, EntityStore,
    EqualityIndexStore, RangeIndexStore, ReadMode, StateView,
};
use arkiv_reth_statemanager::{WriteManager, write_manager};
use arkiv_store::{ArkivDb, NodeStore};
use std::sync::Arc;

/// The Arkiv address — `0x4400…0044`, as an alloy [`Address`].
///
/// EOAs `CALL` here with entity `execute(Operation[])` calldata. There is no
/// precompile object and no bytecode — calls to this address are routed directly
/// in [`arkiv_transact`]. The bytes come from
/// [`arkiv_interfaces::constants::ethereum::ARKIV_RETH_ADDRESS`], which is also what
/// `arkiv-genesis` asserts against; this is the reth-typed view of that one value.
pub const ARKIV_ADDRESS: Address =
    Address::new(arkiv_interfaces::constants::ethereum::ARKIV_RETH_ADDRESS);

/// Flat gas charged per transaction by the fixed-function executor.
const ARKIV_TX_GAS: u64 = 21_000;

/// The minimum gas that passes reth's tx-pool `IntrinsicGasTooLow` check.
///
/// The pool enforces two conditions (its `validate/eth.rs`):
///   `gas_limit >= initial_gas`  (21_000 + 4·zero + 16·nonzero calldata bytes)
///   `gas_limit >= floor_gas`    (EIP-7623 Prague: 21_000 + 10·tokens)
/// where `tokens = zero + 4·nonzero`. Every gas figure this executor reports is
/// floored at `max(initial, floor)` so `eth_estimateGas` always returns a value
/// the pool accepts — otherwise a cheap op (update, delete, …) estimates below
/// the floor and the SDK's send is rejected before it ever executes.
fn intrinsic_gas(data: &[u8]) -> u64 {
    let zero = data.iter().filter(|b| **b == 0).count() as u64;
    let nonzero = data.len() as u64 - zero;
    let tokens = zero + 4 * nonzero;

    let initial = 21_000 + 4 * tokens;
    let floor = 21_000 + 10 * tokens;
    initial.max(floor)
}

/// An alloy `U256` as the spec's [`UserBalance`] bytes.
fn as_balance(value: U256) -> UserBalance {
    UserBalance::from_be_bytes(value.to_be_bytes())
}

/// Map a state-manager fault into the executor's fatal error channel. Faults
/// here are host/store failures — business-rule reverts never travel this path.
fn state_fault<T: core::fmt::Debug, DBError>(
    context: &'static str,
) -> impl FnOnce(T) -> EVMError<DBError> {
    move |e| EVMError::Custom(format!("{context}: {e:?}"))
}

/// The parent-height ref a write view is opened at. reth owns canonicality on
/// this host, so the hash is not threaded through and stays zero.
fn parent_ref(block_number: u64) -> BlockRef {
    BlockRef::new(block_number.saturating_sub(1), Hash::default())
}

/// Open the one view a transaction stages into.
fn open_view<'a, DB: Database, N: NodeStore>(
    db: &'a mut DB,
    nodes: &'a N,
    block_number: u64,
) -> Result<WriteManager<'a, DB, N>, EVMError<DB::Error>> {
    write_manager(db, nodes, parent_ref(block_number)).map_err(state_fault("open view"))
}

/// Close a committed view: make its new trie nodes durable, hand back the
/// Ethereum diff. The nodes are written before the diff can be committed,
/// so whatever root the chain ends up pointing at is always readable.
fn finish_view<DB: Database, N: NodeStore>(
    view: WriteManager<'_, DB, N>,
    nodes: &N,
) -> Result<EvmState, EVMError<DB::Error>> {
    let (overlay, staging) = view.into_parts();
    nodes
        .flush(staging.into_staged())
        .map_err(state_fault("flush trie nodes"))?;
    Ok(overlay.into_state())
}

/// The block-level inputs of a transaction's fee accounting, read off the
/// `BlockEnv` reth built for the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeEnv {
    /// The block's EIP-1559 base fee: burned, and the floor of the effective price.
    pub base_fee: u64,
    /// The block's fee recipient (`coinbase`): receives the priority fee.
    pub beneficiary: Address,
    /// Whether gas is charged at all. reth turns this off (`disable_fee_charge`)
    /// for `eth_call` and `eth_estimateGas`; a mined transaction always pays.
    pub charge: bool,
}

impl FeeEnv {
    /// The fee inputs of `block`, with charging on.
    pub fn from_block(block: &BlockEnv) -> Self {
        Self {
            base_fee: block.basefee,
            beneficiary: block.beneficiary,
            charge: true,
        }
    }

    /// What `tx` pays per gas in this block — revm's effective gas price:
    /// `min(max_fee, base_fee + priority_fee)`, or a legacy tx's `gasPrice`.
    fn effective_gas_price(&self, tx: &TxEnv) -> u128 {
        tx.effective_gas_price(u128::from(self.base_fee))
    }
}

/// Settle `gas_used` gas of `sender`'s transaction the way revm's post-execution
/// does — the sender-side accounting every transaction shape shares:
///
/// - debit `value_out` (for transfers) and `gas_used × effective_gas_price`;
/// - bump the sender's EOA nonce;
/// - credit the beneficiary the tip, `gas_used × (effective_gas_price − base_fee)`.
///   The base-fee share is burned. A zero tip credits nothing, so the beneficiary
///   is not touched into existence as an empty account.
///
/// With [`FeeEnv::charge`] off only the value moves and the nonce bumps, as in
/// revm with `disable_fee_charge`. Only gas actually used is charged; there is no
/// up-front `gas_limit` debit and refund because nothing runs between the two.
fn charge_sender<DB: Database, N: NodeStore>(
    view: &mut WriteManager<'_, DB, N>,
    fees: &FeeEnv,
    sender: Address,
    value_out: U256,
    gas_used: u64,
    tx: &TxEnv,
) -> Result<(), EVMError<DB::Error>> {
    let sender = sender.into_array();
    view.fetch_sub_balance(sender, as_balance(value_out))
        .map_err(state_fault("debit sender value"))?;
    view.fetch_increment_acc_nonce(sender)
        .map_err(state_fault("bump sender nonce"))?;
    if !fees.charge {
        return Ok(());
    }

    let price = fees.effective_gas_price(tx);
    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(price));
    view.fetch_sub_balance(sender, as_balance(gas_cost))
        .map_err(state_fault("debit sender gas"))?;

    let tip = price.saturating_sub(u128::from(fees.base_fee));
    if tip > 0 {
        let reward = U256::from(gas_used).saturating_mul(U256::from(tip));
        view.fetch_add_balance(fees.beneficiary.into_array(), as_balance(reward))
            .map_err(state_fault("credit beneficiary tip"))?;
    }
    Ok(())
}

/// Which of revm's pre-execution fee checks to run. Each maps to a `CfgEnv`
/// flag reth sets for a specific path: `eth_call` / `eth_estimateGas` disable
/// the base-fee check and fee charging; engine-tree payload prewarming disables
/// the balance check (with the nonce and base-fee checks) because its results
/// are cache-only.
#[derive(Debug, Clone, Copy)]
struct FeeChecks {
    base_fee: bool,
    priority_fee: bool,
    balance: bool,
    /// Whether gas will be charged at all (`disable_fee_charge` off). When it
    /// will not, the balance only has to cover the value: revm's
    /// `calculate_caller_fee` returns before its balance check in that case.
    charge: bool,
}

impl FeeChecks {
    /// The `Cfg` accessors, not the fields: each field is behind a revm cargo
    /// feature, and the accessor reads `false` when that feature is off.
    fn from_cfg(cfg: &CfgEnv<SpecId>) -> Self {
        Self {
            base_fee: !cfg.is_base_fee_check_disabled(),
            priority_fee: !cfg.is_priority_fee_check_disabled(),
            balance: !cfg.is_balance_check_disabled(),
            charge: !cfg.is_fee_charge_disabled(),
        }
    }
}

// ---------------------------------------------------------------------------
// The no-EVM execution engine
// ---------------------------------------------------------------------------

/// Arkiv's EVM replacement.
///
/// Wraps reth's stock [`EthEvm`] purely to reuse its database/environment
/// plumbing (block env, cfg, the `State` cache, system-call path). User
/// transactions are executed by [`arkiv_transact`] — the bytecode interpreter is
/// never invoked.
pub struct ArkivEvm<DB: Database, I = NoOpInspector> {
    inner: EthEvm<DB, I, PrecompilesMap>,
    /// The Arkiv database's node store. `None` on a factory built without
    /// one (the non-`node` subcommands), where any Arkiv transaction fails.
    nodes: Option<Arc<ArkivDb>>,
}

impl<DB, I> Evm for ArkivEvm<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>, EthInterpreter>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<SpecId> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    /// The interception point. Applies the state transition directly — **no
    /// interpreter, no `revm` execution of the transaction.**
    ///
    /// Bypassing revm also bypasses its pre-execution validation stage, so the
    /// nonce check has to be re-done here — see [`validate_nonce`].
    fn transact_raw(
        &mut self,
        tx: TxEnv,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
        let block_number = self.inner.block().number.saturating_to::<u64>();
        // Read the flags before taking the mutable database borrow. reth sets
        // `disable_nonce_check` for `eth_simulateV1` without `validation` and for
        // engine-tree payload prewarming, both of which execute against state that
        // need not match the tx's nonce; the fee flags likewise per RPC path.
        let nonce_check = !self.cfg_env().is_nonce_check_disabled();
        let fee_checks = FeeChecks::from_cfg(self.cfg_env());
        let fees = FeeEnv {
            charge: fee_checks.charge,
            ..FeeEnv::from_block(self.inner.block())
        };
        let chain_id = self.inner.chain_id();
        let Some(nodes) = self.nodes.clone() else {
            return Err(EVMError::Custom(
                "the Arkiv database is not configured on this executor".into(),
            ));
        };
        let db = self.inner.db_mut();
        let has_purge_selector = tx.kind == TxKind::Call(ARKIV_ADDRESS)
            && tx.data.starts_with(&purgeExpiredCall::SELECTOR);
        let is_protocol_purge = is_protocol_purge(&tx, block_number, chain_id);
        if has_purge_selector && !is_protocol_purge {
            return Err(invalid_transaction("purgeExpired is protocol-only"));
        }
        if !is_protocol_purge {
            if nonce_check {
                validate_nonce(db, &tx)?;
            }
            validate_fees(db, &tx, fees.base_fee, fee_checks)?;
        }
        arkiv_transact(db, &*nodes, block_number, fees, tx)
    }

    /// System-contract calls (EIP-4788 / EIP-2935) are protocol housekeeping, not
    /// user execution; we delegate them to revm so block validity is preserved.
    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&DB, &I, &PrecompilesMap) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut DB, &mut I, &mut PrecompilesMap) {
        self.inner.components_mut()
    }

    fn finish(self) -> (DB, EvmEnv<SpecId, BlockEnv>) {
        self.inner.finish()
    }
}

fn is_protocol_purge(tx: &TxEnv, block_number: u64, chain_id: u64) -> bool {
    use arkiv_bindings::{PURGE_CALLER, PURGE_GAS_LIMIT};

    tx.tx_type == 2
        && tx.caller == PURGE_CALLER
        && tx.kind == TxKind::Call(ARKIV_ADDRESS)
        && tx.data.starts_with(&purgeExpiredCall::SELECTOR)
        && tx.nonce == block_number
        && tx.chain_id == Some(chain_id)
        && tx.gas_limit == PURGE_GAS_LIMIT
        && tx.gas_price == u128::MAX
        && tx.gas_priority_fee == Some(0)
        && tx.value == U256::ZERO
        && tx.access_list.is_empty()
        && tx.blob_hashes.is_empty()
        && tx.authorization_list.is_empty()
}

fn invalid_transaction<DBError>(message: impl Into<String>) -> EVMError<DBError> {
    EVMError::Transaction(InvalidTransaction::Str(message.into().into()))
}

/// revm's pre-execution nonce check, which the no-EVM path would otherwise skip.
///
/// This is replay protection, and it is the node's only copy of it once revm is out of
/// the picture — the tx-pool's validator does not see blocks arriving over the engine
/// API or P2P.
///
/// Returning [`EVMError::Transaction`] rather than [`EVMError::Custom`] is load-bearing.
/// Only the former is converted to `BlockValidationError::InvalidTx` by
/// `BlockExecutionError::evm`, and only that variant satisfies the
/// `error.is_nonce_too_low()` test reth's payload builder uses to *skip* a transaction
/// instead of aborting the block. Without it, a transaction still sitting in the pool
/// when the dev miner's next interval fires — the pool is cleared asynchronously, on the
/// canonical-state stream — is built into a second block and applied twice.
fn validate_nonce<DB: Database>(db: &mut DB, tx: &TxEnv) -> Result<(), EVMError<DB::Error>> {
    let state = db
        .basic(tx.caller)
        .map_err(EVMError::Database)?
        .map(|acc| acc.nonce)
        .unwrap_or_default();

    match tx.nonce.cmp(&state) {
        Ordering::Equal => Ok(()),
        Ordering::Less => Err(EVMError::Transaction(InvalidTransaction::NonceTooLow {
            tx: tx.nonce,
            state,
        })),
        Ordering::Greater => Err(EVMError::Transaction(InvalidTransaction::NonceTooHigh {
            tx: tx.nonce,
            state,
        })),
    }
}

/// revm's pre-execution fee checks, which the no-EVM path would otherwise skip:
///
/// - the priority fee may not exceed the fee cap;
/// - the effective price may not fall below the block base fee — for a locally
///   built block the pool guarantees this, for a block received over the Engine
///   API or P2P nothing else does;
/// - the sender must be able to afford the maximum spend, `gas_limit × max_fee +
///   value`, the same bound the pool admits on. With fee charging disabled
///   (`eth_call`, `eth_estimateGas`) only the value has to be covered: revm skips
///   the gas bound there, and the value bound keeps [`charge_sender`]'s saturating
///   debit from moving value the sender does not have.
///
/// All three are typed [`InvalidTransaction`]s, for the reason [`validate_nonce`]
/// spells out: the payload builder skips a transaction on those and aborts the
/// block on anything else.
fn validate_fees<DB: Database>(
    db: &mut DB,
    tx: &TxEnv,
    base_fee: u64,
    checks: FeeChecks,
) -> Result<(), EVMError<DB::Error>> {
    let max_fee = tx.max_fee_per_gas();
    if checks.priority_fee && tx.max_priority_fee_per_gas().unwrap_or_default() > max_fee {
        return Err(EVMError::Transaction(
            InvalidTransaction::PriorityFeeGreaterThanMaxFee,
        ));
    }
    if checks.base_fee && tx.effective_gas_price(u128::from(base_fee)) < u128::from(base_fee) {
        return Err(EVMError::Transaction(
            InvalidTransaction::GasPriceLessThanBasefee,
        ));
    }
    if checks.balance {
        let max_spend = if checks.charge {
            U256::from(tx.gas_limit)
                .saturating_mul(U256::from(max_fee))
                .saturating_add(tx.value)
        } else {
            tx.value
        };
        let balance = db
            .basic(tx.caller)
            .map_err(EVMError::Database)?
            .map(|acc| acc.balance)
            .unwrap_or_default();
        if balance < max_spend {
            return Err(EVMError::Transaction(
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(max_spend),
                    balance: Box::new(balance),
                },
            ));
        }
    }
    Ok(())
}

/// The fixed-function state-transition function — Rust, no EVM.
///
/// Callers reach this through [`ArkivEvm::transact_raw`], which validates the nonce
/// and the fee preconditions first; the sender's nonce bump below therefore lands
/// on `tx.nonce + 1`, and the sender can afford the charge.
///
/// Reads accounts through the `Database`, applies the transfer / entity-call
/// accounting, and returns a `ResultAndState` (execution result + the account
/// diff). The caller (`EthBlockExecutor::commit_transaction`) commits the diff
/// into the `State`, producing the `BundleState` reth hashes into the state root.
fn arkiv_transact<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    block_number: u64,
    fees: FeeEnv,
    tx: TxEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    // Neuter: user programs never execute. The rejection must be a *typed*
    // invalid-tx error, for the same reason [`validate_nonce`]'s is: the stock
    // pool accepts deployment transactions, and reth's payload builder skips a
    // transaction only when the error converts into
    // `BlockValidationError::InvalidTx`. An `EVMError::Custom` is fatal to the
    // whole build instead — and since an unmined transaction is never evicted
    // from the pool, one pooled deploy tx would stall block production forever.
    let to = match tx.kind {
        TxKind::Call(addr) => addr,
        TxKind::Create => {
            return Err(EVMError::Transaction(InvalidTransaction::Str(
                "contract creation is disabled (no-EVM Arkiv executor)".into(),
            )));
        }
    };

    // A call to ARKIV_ADDRESS is a read-only view, the user entity transition
    // `execute(Operation[])`, or the protocol-only `purgeExpired(bytes32[])`.
    if to == ARKIV_ADDRESS {
        let selector = tx.data.get(..4).unwrap_or_default();
        if selector == IEntityRegistry::entityNonceCall::SELECTOR {
            return arkiv_entity_nonce_call(db, nodes, &tx, block_number, &fees);
        }
        if selector == IEntityRegistry::customAttributeNamesCall::SELECTOR {
            return arkiv_custom_attribute_names_call(db, nodes, &tx, block_number, &fees);
        }
        if selector == IEntityRegistry::attributeTypeIdCall::SELECTOR {
            return arkiv_attribute_type_id_call(db, nodes, &tx, block_number, &fees);
        }
        if selector == purgeExpiredCall::SELECTOR {
            return arkiv_purge_expired(db, nodes, block_number, &tx);
        }
        return arkiv_entity_transact(db, nodes, block_number, &fees, &tx);
    }

    // Otherwise it's a plain value transfer. 21k flat, floored at the calldata
    // intrinsics should the transfer carry data.
    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    let value_out = if to == tx.caller {
        U256::ZERO
    } else {
        tx.value
    };

    // Sender debit + nonce bump and recipient credit, through one view — a
    // transfer stages the same way every other state change does.
    let mut view = open_view(db, nodes, block_number)?;
    charge_sender(&mut view, &fees, tx.caller, value_out, gas_used, &tx)?;
    if to != tx.caller {
        view.fetch_add_balance(to.into_array(), as_balance(tx.value))
            .map_err(state_fault("credit recipient"))?;
    }
    StateView::commit(&mut view).map_err(state_fault("commit transfer"))?;
    let state = finish_view(view, nodes)?;

    let result = ExecutionResult::Success {
        reason: SuccessReason::Stop,
        gas: ResultGas::default().with_total_gas_spent(gas_used),
        logs: Vec::new(),
        output: Output::Call(Bytes::new()),
    };

    Ok(ResultAndState::new(result, state))
}

fn arkiv_purge_expired<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    block_number: u64,
    tx: &TxEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    use arkiv_bindings::MAX_PURGE_KEYS;
    use arkiv_interfaces::statemanager::EntityUpdates;

    if tx.gas_limit < intrinsic_gas(&tx.data) {
        return Ok(out_of_gas());
    }
    let call = purgeExpiredCall::abi_decode_raw(&tx.data[4..])
        .map_err(|e| invalid_transaction(format!("invalid purgeExpired calldata: {e}")))?;
    if call.entityKeys.len() > MAX_PURGE_KEYS {
        return Err(invalid_transaction(format!(
            "purge contains more than {MAX_PURGE_KEYS} keys"
        )));
    }
    let purge_keys = call.entityKeys;
    let mut view = open_view(db, nodes, block_number)?;
    let mut modeled_gas = 0u64;
    for key in &purge_keys {
        let key_bytes = key.0;
        let Some(entity) = view
            .get_entity(key_bytes, ReadMode::ViewWithOverlay)
            .map_err(state_fault("read purge entity"))?
        else {
            continue;
        };
        if entity.expires_at > block_number {
            continue;
        }
        modeled_gas =
            modeled_gas.saturating_add(arkiv_interfaces::gas::purge_cost(entity.attributes.len()));
        if modeled_gas > tx.gas_limit {
            return Ok(out_of_gas());
        }
        view.update_entity(EntityUpdates::deletion(key_bytes))
            .map_err(state_fault("stage purge deletion"))?;
    }
    let deltas = view
        .get_uncommitted_deltas()
        .map_err(state_fault("purge deltas"))?;
    view.equality_index_mut()
        .apply_deltas(&deltas)
        .map_err(state_fault("purge equality index"))?;
    view.range_index_mut()
        .apply_deltas(&deltas)
        .map_err(state_fault("purge range index"))?;
    StateView::commit(&mut view).map_err(state_fault("commit purge"))?;
    let state = finish_view(view, nodes)?;
    let gas_used = modeled_gas.max(intrinsic_gas(&tx.data));
    Ok(ResultAndState::new(
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default().with_total_gas_spent(gas_used),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        },
        state,
    ))
}

fn out_of_gas() -> ResultAndState<HaltReason> {
    ResultAndState::new(
        ExecutionResult::Halt {
            reason: HaltReason::OutOfGas(OutOfGasError::Basic),
            gas: ResultGas::default(),
            logs: Vec::new(),
        },
        EvmState::default(),
    )
}

/// The entity state transition for a call to [`ARKIV_ADDRESS`].
///
/// Reads the caller's minting nonce, decodes the `Operation[]` calldata, runs the
/// batch over a diff-backed store, and returns the `EvmState` reth commits — plus
/// the sender's gas charge and nonce bump. A business-rule revert charges gas but
/// stages no entity changes; a decode fault reverts likewise.
fn arkiv_entity_transact<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    block_number: u64,
    fees: &FeeEnv,
    tx: &TxEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    // During eth_estimateGas the binary search probes gas limits below the
    // pool's intrinsic minimum. Halt so the search raises its lower bound;
    // otherwise it converges on a value the tx-pool later rejects with
    // IntrinsicGasTooLow. Nothing is staged, so the state stays clean for the
    // next probe.
    let floor = intrinsic_gas(&tx.data);
    if tx.gas_limit < floor {
        return Ok(ResultAndState::new(
            ExecutionResult::Halt {
                reason: HaltReason::OutOfGas(OutOfGasError::Basic),
                gas: ResultGas::default(),
                logs: Vec::new(),
            },
            EvmState::default(),
        ));
    }

    let caller = tx.caller;
    let env = ExecEnv {
        caller: caller.into_array(),
        block_number,
        gas_supplied: tx.gas_limit,
        chain_id: tx.chain_id.unwrap_or(1),
    };

    // One view for the whole transaction: the entity phase and the sender
    // phase stage into the same overlay; a revert stages no entity changes, so
    // the one commit at the end flushes exactly what should land.
    let mut view = open_view(db, nodes, block_number)?;
    let start_nonce = view
        .get_entity_creation_nonce(env.caller, ReadMode::ViewWithOverlay)
        .map_err(state_fault("read minting nonce"))?;
    let outcome = match decode_ops(&env, &tx.data, start_nonce) {
        Ok(ops) => run_ops(&mut view, &env, ops).map_err(|e| EVMError::Custom(e.to_string()))?,
        Err(e) => Outcome {
            gas_used: 0,
            revert: Some(revert::decode_revert_data(&e)),
            logs: Vec::new(),
        },
    };

    // Sender phase: charge gas, bump the EOA nonce. The charged/reported figure
    // is floored at the pool's intrinsic minimum so a cheap batch still
    // estimates to a pool-acceptable gas limit.
    let gas_used = outcome.gas_used.max(floor);
    charge_sender(&mut view, fees, caller, U256::ZERO, gas_used, tx)?;
    StateView::commit(&mut view).map_err(state_fault("commit transaction"))?;
    let evm_state = finish_view(view, nodes)?;

    let gas = ResultGas::default().with_total_gas_spent(gas_used);
    let result = match outcome.revert {
        None => ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas,
            logs: outcome.logs,
            output: Output::Call(Bytes::new()),
        },
        Some(data) => ExecutionResult::Revert {
            gas,
            logs: Vec::new(),
            output: data,
        },
    };
    Ok(ResultAndState::new(result, evm_state))
}

/// Answer a read-only Arkiv view call on `ARKIV_ADDRESS`.
///
/// Every view has the same shape — decode args, read committed state, ABI-encode
/// the return — so the *accounting* around it is written once here rather than
/// per view. `answer` returns `Ok(return_data)` or `Err(revert_data)`.
///
/// The only state staged is the sender's flat gas charge + EOA nonce bump:
/// meaningless for an `eth_call` (the diff is discarded), but it keeps reth's
/// sender invariants intact if the call ever arrives as a mined transaction.
fn arkiv_view_call<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
    answer: impl FnOnce(&mut DB, &N) -> Result<Result<Vec<u8>, Vec<u8>>, EVMError<DB::Error>>,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    let output = answer(db, nodes)?;

    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    let mut view = open_view(db, nodes, block_number)?;
    charge_sender(&mut view, fees, tx.caller, U256::ZERO, gas_used, tx)?;
    StateView::commit(&mut view).map_err(state_fault("commit view call"))?;
    let state = finish_view(view, nodes)?;

    let gas = ResultGas::default().with_total_gas_spent(gas_used);
    let result = match output {
        Ok(ret) => ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas,
            logs: Vec::new(),
            output: Output::Call(Bytes::from(ret)),
        },
        Err(data) => ExecutionResult::Revert {
            gas,
            logs: Vec::new(),
            output: Bytes::from(data),
        },
    };
    Ok(ResultAndState::new(result, state))
}

/// Malformed view arguments revert with the standard `Error(string)` payload.
fn bad_view_args(view: &str, e: impl core::fmt::Display) -> Vec<u8> {
    alloy_sol_types::Revert::from(format!("invalid {view} calldata: {e}")).abi_encode()
}

/// Read a committed entity for a view call.
fn view_entity<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    key: B256,
    block_number: u64,
) -> Result<Option<arkiv_interfaces::entity::Entity>, EVMError<DB::Error>> {
    open_view(db, nodes, block_number)?
        .get_entity(key.0, ReadMode::ViewOnBase)
        .map_err(state_fault("read entity"))
}

/// `entityNonce(owner)`: the owner's entity-key minting nonce, as a `uint64`.
///
/// SDKs `eth_call` this before sending creates to predict the keys the batch
/// will mint (`derive_entity_address(chain_id, owner, nonce + i, salt)`), so it
/// reads the same system-account slot the execute path mints from.
fn arkiv_entity_nonce_call<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(db, nodes, tx, block_number, fees, |db, nodes| {
        let call = match IEntityRegistry::entityNonceCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("entityNonce", e))),
        };
        let nonce = open_view(db, nodes, block_number)?
            .get_entity_creation_nonce(call.owner.into_array(), ReadMode::ViewOnBase)
            .map_err(state_fault("read minting nonce"))?;
        Ok(Ok(IEntityRegistry::entityNonceCall::abi_encode_returns(
            &nonce.get(),
        )))
    })
}

/// `customAttributeNames(entityKey)`: the entity's user attribute names, in the
/// stored (strictly ascending) order.
///
/// System attributes are deliberately absent: they are the same for every
/// entity, so listing them would be noise. A missing or expired entity answers
/// with an empty list rather than reverting — "no attributes" is the truthful
/// answer to "what does this entity have", and it keeps the view total.
fn arkiv_custom_attribute_names_call<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(db, nodes, tx, block_number, fees, |db, nodes| {
        let call = match IEntityRegistry::customAttributeNamesCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("customAttributeNames", e))),
        };
        let names = match live_entity(
            view_entity(db, nodes, call.entityKey, block_number)?,
            block_number,
        ) {
            Some(e) => e.attributes.iter().map(|a| ident32_of(&a.key)).collect(),
            None => Vec::new(),
        };
        Ok(Ok(
            IEntityRegistry::customAttributeNamesCall::abi_encode_returns(&names),
        ))
    })
}

/// `attributeTypeId(entityKey, name)`: the `typeId` that attribute holds, or
/// **0** if it is not set.
///
/// 0 is [`TOMBSTONE_TYPE_ID`](arkiv_interfaces::entity::TOMBSTONE_TYPE_ID) — the
/// tag that means "unset" on the wire — so "absent" reads the same here as it
/// does in a patch. No type ever has id 0, so the answer stays unambiguous.
fn arkiv_attribute_type_id_call<DB: Database, N: NodeStore>(
    db: &mut DB,
    nodes: &N,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(db, nodes, tx, block_number, fees, |db, nodes| {
        let call = match IEntityRegistry::attributeTypeIdCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("attributeTypeId", e))),
        };
        let wanted = strip_trailing_zeros(call.name.0.to_vec());
        let type_id = live_entity(
            view_entity(db, nodes, call.entityKey, block_number)?,
            block_number,
        )
        .and_then(|e| {
            e.attributes
                .iter()
                .find(|a| a.key == wanted)
                .map(|a| a.value.type_id())
        })
        .unwrap_or(arkiv_interfaces::entity::TOMBSTONE_TYPE_ID);
        Ok(Ok(
            IEntityRegistry::attributeTypeIdCall::abi_encode_returns(&type_id),
        ))
    })
}

/// Hide a past-expiry entity from the views, matching the `arkiv_*` read rule:
/// an entity is expired iff `expires_at <= current`, whether or not it has been
/// physically removed yet.
fn live_entity(
    entity: Option<arkiv_interfaces::entity::Entity>,
    block_number: u64,
) -> Option<arkiv_interfaces::entity::Entity> {
    entity.filter(|e| e.expires_at > block_number)
}

/// An attribute name back into the ABI's fixed-width, null-padded `Ident32`.
fn ident32_of(name: &[u8]) -> alloy_primitives::FixedBytes<32> {
    let mut w = [0u8; 32];
    let n = name.len().min(32);
    w[..n].copy_from_slice(&name[..n]);
    alloy_primitives::FixedBytes::from(w)
}

fn strip_trailing_zeros(mut v: Vec<u8>) -> Vec<u8> {
    while matches!(v.last(), Some(0)) {
        v.pop();
    }
    v
}

/// What running an op batch produced: the gas metered, the ABI-encoded revert
/// payload if the batch failed a business rule, and the `EntityOperation` logs
/// to emit (empty on revert). The state itself needs no field here — it is
/// staged in the [`WriteManager`] the batch ran over.
struct Outcome {
    gas_used: u64,
    revert: Option<Bytes>,
    logs: Vec<Log>,
}

/// Run a decoded batch through the write view: on success, fold the staged
/// deltas into the index stores and advance the minting nonce per create; on a
/// revert nothing is staged.
fn run_ops<DB: Database, N: NodeStore>(
    view: &mut WriteManager<'_, DB, N>,
    env: &ExecEnv,
    ops: Vec<Op>,
) -> Result<Outcome, eyre::Report> {
    let create_count = ops
        .iter()
        .filter(|o| matches!(o, Op::Create { .. }))
        .count() as u64;

    let mut effects = Vec::new();
    let out = ArkivExecutor::with_cost(*view.cost_model())
        .apply_with_effects(env, view, &ops, &mut effects)
        .map_err(|e| eyre::eyre!("apply: {e:?}"))?;

    match out.status {
        ExecStatus::Ok => {
            let deltas = view
                .get_uncommitted_deltas()
                .map_err(|e| eyre::eyre!("uncommitted deltas: {e:?}"))?;
            view.equality_index_mut()
                .apply_deltas(&deltas)
                .map_err(|e| eyre::eyre!("equality index apply_deltas: {e:?}"))?;
            view.range_index_mut()
                .apply_deltas(&deltas)
                .map_err(|e| eyre::eyre!("range index apply_deltas: {e:?}"))?;
            for _ in 0..create_count {
                view.fetch_increment_entity_creation_nonce(env.caller)
                    .map_err(|e| eyre::eyre!("advance minting nonce: {e:?}"))?;
            }
            let logs = effects.iter().map(entity_operation_log).collect();
            Ok(Outcome {
                gas_used: out.gas_used,
                revert: None,
                logs,
            })
        }
        ExecStatus::Reverted => Ok(Outcome {
            gas_used: out.gas_used,
            revert: out.revert.as_ref().map(revert::revert_data),
            logs: Vec::new(),
        }),
    }
}

/// Encode an [`OpEffect`] as the ABI event log a client indexes.
///
/// One event per op kind, rather than one generic event with a discriminator:
/// each carries only the fields that op actually changes, and an indexer can
/// filter on the topic instead of decoding every entity write to find out
/// whether it cared.
fn entity_operation_log(effect: &OpEffect) -> Log {
    use arkiv_bindings::IEntityRegistry as E;
    let key = B256::from(effect.key);
    let owner = Address::from(effect.owner);
    let data = match effect.kind {
        OpKind::Create => E::EntityCreated {
            entityKey: key,
            owner,
            expiresAt: effect.expires_at,
            creationFlags: effect.creation_flags.bits(),
        }
        .encode_log_data(),
        OpKind::Patch => E::EntityPatched {
            entityKey: key,
            owner,
        }
        .encode_log_data(),
        OpKind::ExtendExpiry => E::ExpiryExtended {
            entityKey: key,
            owner,
            expiresAt: effect.expires_at,
        }
        .encode_log_data(),
        OpKind::Transfer => E::OwnershipTransferred {
            entityKey: key,
            previousOwner: Address::from(effect.previous_owner.unwrap_or(effect.owner)),
            newOwner: owner,
        }
        .encode_log_data(),
        OpKind::Delete => E::EntityDeleted {
            entityKey: key,
            owner,
        }
        .encode_log_data(),
    };
    Log {
        address: ARKIV_ADDRESS,
        data,
    }
}

// ---------------------------------------------------------------------------
// Factory + executor builder (the reth injection points)
// ---------------------------------------------------------------------------

/// Custom EVM factory for Arkiv: builds [`ArkivEvm`] (the no-EVM engine) on top
/// of reth's stock EVM context.
#[derive(Debug, Clone, Default)]
pub struct ArkivEvmFactory {
    /// The Arkiv database. `None` builds an executor that rejects every
    /// Arkiv transaction, for the subcommands that never execute one.
    nodes: Option<Arc<ArkivDb>>,
}

impl ArkivEvmFactory {
    pub fn new(nodes: Arc<ArkivDb>) -> Self {
        Self { nodes: Some(nodes) }
    }
}

impl EvmFactory for ArkivEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> = ArkivEvm<DB, I>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        // Stock precompiles, carried but unused — the interpreter never runs.
        let inner = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(Precompiles::prague()));

        ArkivEvm {
            inner: EthEvm::new(inner, false),
            nodes: self.nodes.clone(),
        }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let inner = self
            .create_evm(db, input)
            .inner
            .into_inner()
            .with_inspector(inspector);
        ArkivEvm {
            inner: EthEvm::new(inner, true),
            nodes: self.nodes.clone(),
        }
    }
}

/// Where the Arkiv database lives under a reth datadir.
pub fn arkiv_db_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("arkiv-db")
}

/// Builds the Arkiv block executor: reth's stock Ethereum block executor driven
/// by [`ArkivEvmFactory`]. This is the type arkiv-reth hands to
/// `EthereumNode::components().executor(..)`.
///
/// Generic over the node's chain spec with the same bounds reth's own
/// `EthereumExecutorBuilder` asks for, so the node can run on arkiv-reth-chainspec's
/// `ArkivChainSpec` (the minimum-base-fee rule) as well as on reth's `ChainSpec`.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct ArkivExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for ArkivExecutorBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthExecutorSpec + EthereumHardforks,
            Primitives = EthPrimitives,
        >,
    >,
{
    type EVM = EthEvmConfig<<Node::Types as NodeTypes>::ChainSpec, ArkivEvmFactory>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        tracing::info!(
            target: "arkiv::executor",
            arkiv_address = %ARKIV_ADDRESS,
            "Assembling Arkiv no-EVM executor over reth host (interpreter bypassed)",
        );
        let nodes = ArkivDb::shared(
            &arkiv_db_path(ctx.config().datadir().data_dir()),
            tokio::runtime::Handle::current(),
        )?;
        tracing::info!(target: "arkiv::executor", path = %nodes.path().display(), "opened the Arkiv database");
        Ok(EthEvmConfig::new_with_evm_factory(
            ctx.chain_spec(),
            ArkivEvmFactory::new(nodes),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use alloy_sol_types::SolCall;
    use arkiv_bindings::{IEntityRegistry, Operation};
    use arkiv_interfaces::primitives::EntityCreationNonce;
    use arkiv_store::{ARKIV_ROOT_ACCOUNT, ARKIV_ROOT_SLOT, DbView, SharedMemNodeStore};

    /// The database as the returned diff's anchor slot names it.
    fn db_view<'a>(
        state: &EvmState,
        nodes: &'a SharedMemNodeStore,
    ) -> DbView<'a, SharedMemNodeStore> {
        let root = state
            .get(&ARKIV_ROOT_ACCOUNT)
            .and_then(|acc| acc.storage.get(&ARKIV_ROOT_SLOT))
            .map(|slot| B256::from(slot.present_value.to_be_bytes::<32>()))
            .unwrap_or_default();
        DbView::open(nodes, root).expect("the diff names a root the store holds")
    }
    use reth_ethereum::evm::revm::database_interface::EmptyDB;

    /// A create with a purely relative lifetime of `min_lifetime` blocks,
    /// carrying `payload` as the `$payload` triple.
    fn create_calldata(min_lifetime: u64, payload: &'static [u8]) -> Bytes {
        let payload_attr = arkiv_bindings::Attribute::from_value(
            arkiv_bindings::Ident32::system("$payload").unwrap(),
            &arkiv_interfaces::entity::AttributeValue::Bytes(payload.to_vec()),
        )
        .unwrap();
        IEntityRegistry::executeCall {
            ops: vec![Operation::create(0, 0, min_lifetime, 0, vec![payload_attr])],
        }
        .abi_encode()
        .into()
    }

    /// No base fee, no beneficiary: the tests above are about entity semantics,
    /// and `arkiv_tx` prices at zero, so nothing is charged or rewarded.
    const NO_FEES: FeeEnv = FeeEnv {
        base_fee: 0,
        beneficiary: Address::ZERO,
        charge: true,
    };

    fn arkiv_tx(caller: Address, data: Bytes) -> TxEnv {
        TxEnv {
            caller,
            gas_limit: 1_000_000,
            gas_price: 0,
            kind: TxKind::Call(ARKIV_ADDRESS),
            data,
            chain_id: Some(1),
            ..Default::default()
        }
    }

    fn protocol_purge_tx(block: u64, keys: Vec<B256>) -> TxEnv {
        use arkiv_bindings::{PURGE_CALLER, PURGE_GAS_LIMIT};

        TxEnv {
            tx_type: 2,
            caller: PURGE_CALLER,
            gas_limit: PURGE_GAS_LIMIT,
            gas_price: u128::MAX,
            kind: TxKind::Call(ARKIV_ADDRESS),
            data: purgeExpiredCall { entityKeys: keys }.abi_encode().into(),
            nonce: block,
            chain_id: Some(1),
            gas_priority_fee: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn protocol_purge_requires_the_exact_envelope() {
        let tx = protocol_purge_tx(10, Vec::new());
        assert!(is_protocol_purge(&tx, 10, 1));

        let mut user = tx.clone();
        user.caller = Address::repeat_byte(0xAA);
        assert!(!is_protocol_purge(&user, 10, 1));

        let mut wrong_nonce = tx.clone();
        wrong_nonce.nonce += 1;
        assert!(!is_protocol_purge(&wrong_nonce, 10, 1));

        let mut wrong_gas = tx;
        wrong_gas.gas_limit -= 1;
        assert!(!is_protocol_purge(&wrong_gas, 10, 1));
    }

    #[test]
    fn malformed_and_oversized_purges_are_invalid_transactions() {
        use alloy_evm::EvmError;

        let mut db = EmptyDB::default();

        let nodes = SharedMemNodeStore::new();
        let mut malformed = protocol_purge_tx(10, Vec::new());
        malformed.data = purgeExpiredCall::SELECTOR.into();
        let error = arkiv_purge_expired(&mut db, &nodes, 10, &malformed).unwrap_err();
        assert!(error.try_into_invalid_tx_err().is_ok());

        let oversized = protocol_purge_tx(
            10,
            (0..=arkiv_bindings::MAX_PURGE_KEYS)
                .map(|i| B256::repeat_byte(i as u8))
                .collect(),
        );
        let error = arkiv_purge_expired(&mut db, &nodes, 10, &oversized).unwrap_err();
        assert!(error.try_into_invalid_tx_err().is_ok());
    }

    #[test]
    fn protocol_purge_skips_a_live_entity() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let created = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_calldata(50, b"live")),
        )
        .unwrap();
        db.commit(created.state);
        let key = B256::from(derive_entity_address(
            1,
            &alice.into_array(),
            EntityCreationNonce::new(0),
            0,
        ));

        let purged =
            arkiv_purge_expired(&mut db, &nodes, 11, &protocol_purge_tx(11, vec![key])).unwrap();
        assert!(purged.result.is_success());
        assert!(
            !purged.state.contains_key(&ARKIV_ROOT_ACCOUNT),
            "a live entity must not be staged for deletion: the root is untouched"
        );
    }

    /// A create call through `arkiv_transact`: the entity is committed at its minted
    /// key, the sender is charged/bumped, and the minting nonce advances — all in the
    /// returned `EvmState`.
    #[test]
    fn entity_create_call_commits_the_entity() {
        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_calldata(50, b"hello")),
        )
        .unwrap();

        assert!(rs.result.is_success());

        // The entity landed at the derived key in the database the diff's
        // anchor slot names, with env-resolved fields.
        let key = derive_entity_address(1, &[0xAA; 20], EntityCreationNonce::new(0), 0);
        let view = db_view(&rs.state, &nodes);
        let entity = view.entity(&key).unwrap().expect("entity in the database");
        assert_eq!(entity.owner, [0xAA; 20]);
        assert_eq!(entity.expires_at, 60); // block 10 + minLifetime 50
        assert_eq!(entity.payload, b"hello");

        // The minting nonce advanced to 1.
        assert_eq!(view.creation_nonce(&alice.into_array()).unwrap().get(), 1);

        // The sender is touched with its EOA nonce bumped.
        let sender = rs.state.get(&alice).expect("sender account");
        assert_eq!(sender.info.nonce, 1);

        // One EntityCreated log was emitted for the create, at ARKIV_ADDRESS.
        let logs = rs.result.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, ARKIV_ADDRESS);
        let event =
            arkiv_bindings::IEntityRegistry::EntityCreated::decode_log_data(&logs[0].data).unwrap();
        assert_eq!(event.entityKey, B256::from(key));
        assert_eq!(event.owner, alice);
        assert_eq!(event.expiresAt, 60);
        assert_eq!(event.creationFlags, 0);
    }

    /// **Replay protection.** Re-submitting a transaction that has already been
    /// mined must be rejected, not applied a second time.
    ///
    /// Without this check the whole `Operation[]` batch re-runs against
    /// post-first-execution state. For a create that is not even idempotent: the
    /// minting nonce has advanced, so the replay mints a *second* entity under a
    /// different key from a single user intent.
    #[test]
    fn a_replayed_transaction_is_rejected() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        // CacheDB, not EmptyDB: the point is what the *second* application sees.
        let mut db = CacheDB::new(EmptyDB::default());
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let tx = arkiv_tx(alice, create_calldata(50, b"hello"));

        // First submission: valid at nonce 0, and it advances the sender to 1.
        validate_nonce(&mut db, &tx).expect("a fresh account's nonce 0 is valid");
        let rs = arkiv_transact(&mut db, &nodes, 10, NO_FEES, tx.clone()).unwrap();
        assert!(rs.result.is_success());
        assert_eq!(rs.state.get(&alice).unwrap().info.nonce, 1);
        db.commit(rs.state);

        // The identical transaction, submitted again.
        let err = validate_nonce(&mut db, &tx).expect_err("a replay must be rejected");
        assert!(
            matches!(
                err,
                EVMError::Transaction(InvalidTransaction::NonceTooLow { tx: 0, state: 1 })
            ),
            "expected NonceTooLow {{ tx: 0, state: 1 }}, got {err:?}",
        );
    }

    /// A nonce ahead of the account's is rejected too — the gap has to be filled
    /// before the transaction is executable.
    #[test]
    fn a_future_nonce_is_rejected() {
        let mut db = EmptyDB::default();
        let alice = Address::repeat_byte(0xAA);
        let tx = TxEnv {
            nonce: 7,
            ..arkiv_tx(alice, create_calldata(50, b"hello"))
        };

        let err = validate_nonce(&mut db, &tx).expect_err("a nonce gap must be rejected");
        assert!(
            matches!(
                err,
                EVMError::Transaction(InvalidTransaction::NonceTooHigh { tx: 7, state: 0 })
            ),
            "expected NonceTooHigh {{ tx: 7, state: 0 }}, got {err:?}",
        );
    }

    /// The rejection must be the *variant reth tests for*, not merely some error.
    ///
    /// reth's payload builder skips a transaction only when
    /// `error.is_nonce_too_low()` holds, which requires the error to survive
    /// `BlockExecutionError::evm`'s conversion into `BlockValidationError::InvalidTx`
    /// — something [`EVMError::Custom`] does not do. Weaken this and a stale
    /// transaction the pool has not evicted yet stops being skipped and starts
    /// being built into a second block again.
    #[test]
    fn a_stale_nonce_is_the_error_reths_payload_builder_skips_on() {
        use alloy_evm::{EvmError, InvalidTxError};
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let tx = arkiv_tx(alice, create_calldata(50, b"hello"));
        let rs = arkiv_transact(&mut db, &nodes, 10, NO_FEES, tx.clone()).unwrap();
        db.commit(rs.state);

        let err = validate_nonce(&mut db, &tx).expect_err("a replay must be rejected");
        let invalid = err
            .try_into_invalid_tx_err()
            .expect("must convert to an invalid-tx error, or reth aborts the block instead");
        assert!(
            invalid.is_nonce_too_low(),
            "must satisfy is_nonce_too_low(), or reth stops skipping the duplicate",
        );
    }

    /// **Contract creation is rejected as a typed invalid-tx error, not a fatal
    /// one.** The stock pool accepts deployment transactions and never evicts an
    /// unmined one, so the executor's rejection is what the payload builder
    /// sees on every build — an error that fails
    /// [`try_into_invalid_tx_err`](alloy_evm::EvmError::try_into_invalid_tx_err)
    /// (as [`EVMError::Custom`] does) aborts the whole build, and one pooled
    /// deploy tx stalls block production forever. Weaken this back to `Custom`
    /// and that stall returns.
    #[test]
    fn a_create_transaction_is_rejected_as_invalid_not_fatal() {
        use alloy_evm::{EvmError, InvalidTxError};

        let mut db = EmptyDB::default();

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let tx = TxEnv {
            kind: TxKind::Create,
            ..arkiv_tx(alice, Bytes::from_static(&[0x00]))
        };

        let err =
            arkiv_transact(&mut db, &nodes, 10, NO_FEES, tx).expect_err("creates must be rejected");
        let invalid = err.try_into_invalid_tx_err().expect(
            "must convert to an invalid-tx error, or reth aborts the build instead of skipping",
        );
        assert!(
            !invalid.is_nonce_too_low(),
            "a create rejection is not a nonce problem",
        );
        assert!(
            invalid
                .to_string()
                .contains("contract creation is disabled"),
            "the rejection should say why, got: {invalid}",
        );
    }

    /// A create commits the **index** alongside the entity: the new entity is
    /// found under its `$owner`, and the entity trie lists it.
    #[test]
    fn entity_create_commits_the_index() {
        use arkiv_interfaces::entity::{AttributeValue, annotations};

        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_calldata(50, b"hello")),
        )
        .unwrap();
        assert!(rs.result.is_success());

        let key = derive_entity_address(1, &[0xAA; 20], EntityCreationNonce::new(0), 0);
        let view = db_view(&rs.state, &nodes);
        let owned = view
            .equal(
                annotations::OWNER,
                &AttributeValue::EthereumAddress(alice.into_array()),
            )
            .unwrap();
        assert_eq!(owned, vec![key]);
        let all: Vec<_> = view.entity_keys().unwrap().map(Result::unwrap).collect();
        assert_eq!(all, vec![key]);
    }

    fn nonces_calldata(owner: Address) -> Bytes {
        IEntityRegistry::entityNonceCall { owner }
            .abi_encode()
            .into()
    }

    /// Decode the `uint64` a successful `entityNonce(address)` call returned.
    fn nonce_from(rs: &ResultAndState<HaltReason>) -> u64 {
        assert!(rs.result.is_success());
        IEntityRegistry::entityNonceCall::abi_decode_returns(rs.result.output().unwrap())
            .expect("uint64 return")
    }

    /// `nonces(owner)` on a fresh chain answers 0 — and stages nothing beyond
    /// the sender, regardless of who asks about whom.
    #[test]
    fn nonces_call_returns_zero_for_fresh_owner() {
        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let bob = Address::repeat_byte(0xBB);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(bob, nonces_calldata(alice)),
        )
        .unwrap();

        assert_eq!(nonce_from(&rs), 0);
        // Only the sender (charged/bumped) is in the diff.
        assert_eq!(rs.state.len(), 1);
        assert_eq!(rs.state.get(&bob).expect("sender").info.nonce, 1);
    }

    // ── Entity views ──────────────────────────────────────────────────

    /// A create carrying two user attributes plus the system triples.
    fn create_with_attrs_calldata(min_lifetime: u64) -> Bytes {
        use arkiv_interfaces::entity::AttributeValue;
        let attr = |n: &str, v: AttributeValue| {
            arkiv_bindings::Attribute::from_value(arkiv_bindings::Ident32::encode(n).unwrap(), &v)
                .unwrap()
        };
        IEntityRegistry::executeCall {
            ops: vec![Operation::create(
                0,
                0,
                min_lifetime,
                0,
                vec![
                    attr("rank", AttributeValue::u256_from_u64(7)),
                    attr("color", AttributeValue::Str("blue".into())),
                ],
            )],
        }
        .abi_encode()
        .into()
    }

    fn names_from(rs: &ResultAndState<HaltReason>) -> Vec<String> {
        assert!(rs.result.is_success());
        IEntityRegistry::customAttributeNamesCall::abi_decode_returns(rs.result.output().unwrap())
            .expect("Ident32[] return")
            .iter()
            .map(|n| arkiv_bindings::Ident32::from_word(*n).decode().unwrap())
            .collect()
    }

    fn type_id_from(rs: &ResultAndState<HaltReason>) -> u8 {
        assert!(rs.result.is_success());
        IEntityRegistry::attributeTypeIdCall::abi_decode_returns(rs.result.output().unwrap())
            .expect("uint8 return")
    }

    fn names_calldata(key: B256) -> Bytes {
        IEntityRegistry::customAttributeNamesCall { entityKey: key }
            .abi_encode()
            .into()
    }

    fn type_id_calldata(key: B256, name: &str) -> Bytes {
        IEntityRegistry::attributeTypeIdCall {
            entityKey: key,
            name: arkiv_bindings::Ident32::encode(name).unwrap().into_word(),
        }
        .abi_encode()
        .into()
    }

    /// `customAttributeNames` enumerates the entity's *user* attributes, in the
    /// stored ascending order — system attributes stay out, since they are the
    /// same for every entity and would be pure noise.
    #[test]
    fn custom_attribute_names_lists_user_attributes_in_order() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_with_attrs_calldata(50)),
        )
        .unwrap();
        assert!(rs.result.is_success());
        db.commit(rs.state);

        let key = B256::from(derive_entity_address(
            1,
            &[0xAA; 20],
            EntityCreationNonce::new(0),
            0,
        ));
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            11,
            NO_FEES,
            arkiv_tx(alice, names_calldata(key)),
        )
        .unwrap();
        assert_eq!(names_from(&rs), vec!["color", "rank"]);
    }

    /// `attributeTypeId` answers the stored `typeId`, and **0** for an
    /// attribute that isn't set — the same tag that means "unset" in a patch,
    /// so absence reads identically on both paths. No type has id 0.
    #[test]
    fn attribute_type_id_answers_zero_when_unset() {
        use arkiv_interfaces::entity::{AttributeType, TOMBSTONE_TYPE_ID};
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_with_attrs_calldata(50)),
        )
        .unwrap();
        assert!(rs.result.is_success());
        db.commit(rs.state);

        let key = B256::from(derive_entity_address(
            1,
            &[0xAA; 20],
            EntityCreationNonce::new(0),
            0,
        ));

        let rs = arkiv_transact(
            &mut db,
            &nodes,
            11,
            NO_FEES,
            arkiv_tx(alice, type_id_calldata(key, "rank")),
        )
        .unwrap();
        assert_eq!(type_id_from(&rs), AttributeType::U256.id());
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            11,
            NO_FEES,
            arkiv_tx(alice, type_id_calldata(key, "color")),
        )
        .unwrap();
        assert_eq!(type_id_from(&rs), AttributeType::Str.id());

        // Never set on this entity.
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            11,
            NO_FEES,
            arkiv_tx(alice, type_id_calldata(key, "absent")),
        )
        .unwrap();
        assert_eq!(type_id_from(&rs), TOMBSTONE_TYPE_ID);
    }

    /// A missing entity answers empty / 0 rather than reverting: "nothing" is
    /// the truthful answer to "what does this entity have", and it keeps both
    /// views total so a client never has to distinguish revert-from-empty.
    #[test]
    fn views_are_total_for_a_missing_entity() {
        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let ghost = B256::repeat_byte(0xEE);

        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, names_calldata(ghost)),
        )
        .unwrap();
        assert!(names_from(&rs).is_empty());
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, type_id_calldata(ghost, "rank")),
        )
        .unwrap();
        assert_eq!(type_id_from(&rs), 0);
    }

    /// Past its expiry an entity is invisible to the views too, matching the
    /// `arkiv_*` read rule — otherwise these two would keep answering for
    /// entities every other read path already treats as gone.
    #[test]
    fn views_hide_an_expired_entity() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        // Created at block 10 with a 50-block lifetime → expires_at 60.
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_with_attrs_calldata(50)),
        )
        .unwrap();
        assert!(rs.result.is_success());
        db.commit(rs.state);
        let key = B256::from(derive_entity_address(
            1,
            &[0xAA; 20],
            EntityCreationNonce::new(0),
            0,
        ));

        // Last live block is 59.
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            59,
            NO_FEES,
            arkiv_tx(alice, names_calldata(key)),
        )
        .unwrap();
        assert_eq!(names_from(&rs).len(), 2, "live at 59");

        let rs = arkiv_transact(
            &mut db,
            &nodes,
            60,
            NO_FEES,
            arkiv_tx(alice, names_calldata(key)),
        )
        .unwrap();
        assert!(names_from(&rs).is_empty(), "expired at 60");
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            60,
            NO_FEES,
            arkiv_tx(alice, type_id_calldata(key, "rank")),
        )
        .unwrap();
        assert_eq!(type_id_from(&rs), 0, "expired at 60");
    }

    /// After a create, `nonces` reports 1 for the creator — and still 0 for
    /// anyone else, proving the *decoded argument* is read, not the caller.
    #[test]
    fn nonces_call_reflects_minted_creates() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let bob = Address::repeat_byte(0xBB);

        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_calldata(50, b"hello")),
        )
        .unwrap();
        assert!(rs.result.is_success());
        db.commit(rs.state);

        let rs = arkiv_transact(
            &mut db,
            &nodes,
            11,
            NO_FEES,
            arkiv_tx(bob, nonces_calldata(alice)),
        )
        .unwrap();
        assert_eq!(nonce_from(&rs), 1);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            11,
            NO_FEES,
            arkiv_tx(alice, nonces_calldata(bob)),
        )
        .unwrap();
        assert_eq!(nonce_from(&rs), 0);
    }

    /// The `nonces` selector with truncated arguments reverts.
    #[test]
    fn nonces_call_with_malformed_args_reverts() {
        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let mut data = IEntityRegistry::entityNonceCall::SELECTOR.to_vec();
        data.extend_from_slice(&[0x01, 0x02]);
        let rs =
            arkiv_transact(&mut db, &nodes, 10, NO_FEES, arkiv_tx(alice, data.into())).unwrap();
        assert!(!rs.result.is_success());
    }

    /// The reported gas never falls below the tx-pool's intrinsic minimum
    /// (`max(21000 + 4·tokens, 21000 + 10·tokens)`), even when the cost model
    /// prices the batch cheaper — otherwise eth_estimateGas quotes a limit the
    /// pool rejects as IntrinsicGasTooLow.
    #[test]
    fn reported_gas_is_floored_at_the_intrinsic_minimum() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());

        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, create_calldata(50, b"hello")),
        )
        .unwrap();
        db.commit(rs.state);

        // A patch batch: cheap in the cost model (40k base) but with calldata
        // whose intrinsic floor exceeds it.
        let key = B256::from(derive_entity_address(
            1,
            &[0xAA; 20],
            EntityCreationNonce::new(0),
            0,
        ));
        let big_payload = arkiv_bindings::Attribute::from_value(
            arkiv_bindings::Ident32::system("$payload").unwrap(),
            // 4k nonzero bytes → floor ≈ 181k
            &arkiv_interfaces::entity::AttributeValue::Bytes(vec![0xAB; 4_000]),
        )
        .unwrap();
        let update = IEntityRegistry::executeCall {
            ops: vec![Operation::patch(key, vec![big_payload])],
        }
        .abi_encode();
        let floor = intrinsic_gas(&update);
        let rs =
            arkiv_transact(&mut db, &nodes, 11, NO_FEES, arkiv_tx(alice, update.into())).unwrap();

        assert!(rs.result.is_success());
        assert!(
            rs.result.tx_gas_used() >= floor,
            "gas_used {} must cover the intrinsic floor {floor}",
            rs.result.tx_gas_used(),
        );
    }

    /// A gas limit below the intrinsic floor halts out-of-gas without staging
    /// anything — the shape eth_estimateGas's binary search needs to raise its
    /// lower bound.
    #[test]
    fn gas_limit_below_intrinsic_floor_halts() {
        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let data = create_calldata(50, b"hello");
        let mut tx = arkiv_tx(alice, data.clone());
        tx.gas_limit = intrinsic_gas(&data) - 1;
        let rs = arkiv_transact(&mut db, &nodes, 10, NO_FEES, tx).unwrap();

        assert!(matches!(
            rs.result,
            ExecutionResult::Halt {
                reason: HaltReason::OutOfGas(_),
                ..
            }
        ));
        assert!(rs.state.is_empty(), "a below-floor probe stages nothing");
    }

    /// Undecodable calldata reverts, but the sender is still charged/bumped and no
    /// entity is staged.
    #[test]
    fn undecodable_call_reverts_but_charges_sender() {
        let mut db = EmptyDB::default();
        let nodes = SharedMemNodeStore::new();
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            NO_FEES,
            arkiv_tx(alice, Bytes::from_static(&[0xDE, 0xAD])),
        )
        .unwrap();

        assert!(!rs.result.is_success());
        assert_eq!(rs.state.get(&alice).expect("sender").info.nonce, 1);
        // Nothing else was staged (only the sender).
        assert_eq!(rs.state.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Fee accounting: EIP-1559 as revm applies it
    // -----------------------------------------------------------------------

    use reth_ethereum::evm::revm::{db::CacheDB, state::AccountInfo};

    const ETH: u128 = 1_000_000_000_000_000_000;

    fn funded_db(accounts: &[(Address, u128)]) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for (addr, wei) in accounts {
            db.insert_account_info(
                *addr,
                AccountInfo {
                    balance: U256::from(*wei),
                    ..Default::default()
                },
            );
        }
        db
    }

    /// A 21k plain transfer. `priority` = Some makes it EIP-1559 with `max_fee` as
    /// the fee cap; None makes it legacy with `max_fee` as the gas price.
    fn transfer_tx(
        from: Address,
        to: Address,
        value: u128,
        max_fee: u128,
        priority: Option<u128>,
    ) -> TxEnv {
        TxEnv {
            tx_type: if priority.is_some() { 2 } else { 0 },
            caller: from,
            gas_limit: ARKIV_TX_GAS,
            gas_price: max_fee,
            gas_priority_fee: priority,
            kind: TxKind::Call(to),
            value: U256::from(value),
            chain_id: Some(1),
            ..Default::default()
        }
    }

    fn balance_in(rs: &ResultAndState<HaltReason>, addr: Address) -> U256 {
        rs.state
            .get(&addr)
            .map(|acc| acc.info.balance)
            .unwrap_or_default()
    }

    const ALL_CHECKS: FeeChecks = FeeChecks {
        base_fee: true,
        priority_fee: true,
        balance: true,
        charge: true,
    };

    /// The sender pays `gas_used × min(max_fee, base_fee + tip)`, not the fee
    /// cap; the base-fee share is burned and the tip reaches the beneficiary.
    #[test]
    fn eip1559_sender_pays_the_effective_price_and_the_tip_goes_to_the_beneficiary() {
        let (alice, bob, carol) = (
            Address::repeat_byte(0xAA),
            Address::repeat_byte(0xBB),
            Address::repeat_byte(0xCC),
        );
        let mut db = funded_db(&[(alice, ETH)]);
        let nodes = SharedMemNodeStore::new();
        let fees = FeeEnv {
            base_fee: 5,
            beneficiary: bob,
            charge: true,
        };
        let tx = transfer_tx(alice, carol, 100, 10, Some(3));
        validate_fees(&mut db, &tx, fees.base_fee, ALL_CHECKS).unwrap();
        let rs = arkiv_transact(&mut db, &nodes, 10, fees, tx).unwrap();
        assert!(rs.result.is_success());

        // effective = min(10, 5 + 3) = 8 per gas; tip = 8 - 5 = 3 per gas.
        let gas = u128::from(ARKIV_TX_GAS);
        assert_eq!(balance_in(&rs, alice), U256::from(ETH - 100 - gas * 8));
        assert_eq!(balance_in(&rs, carol), U256::from(100));
        assert_eq!(balance_in(&rs, bob), U256::from(gas * 3));
    }

    /// A fee cap below `base_fee + tip` caps the price, and the tip shrinks to
    /// whatever is left above the base fee.
    #[test]
    fn the_fee_cap_bounds_the_effective_price() {
        let (alice, bob, carol) = (
            Address::repeat_byte(0xAA),
            Address::repeat_byte(0xBB),
            Address::repeat_byte(0xCC),
        );
        let mut db = funded_db(&[(alice, ETH)]);
        let nodes = SharedMemNodeStore::new();
        let fees = FeeEnv {
            base_fee: 5,
            beneficiary: bob,
            charge: true,
        };
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            fees,
            transfer_tx(alice, carol, 0, 6, Some(3)),
        )
        .unwrap();

        let gas = u128::from(ARKIV_TX_GAS);
        assert_eq!(balance_in(&rs, alice), U256::from(ETH - gas * 6));
        assert_eq!(balance_in(&rs, bob), U256::from(gas));
    }

    /// A legacy transaction pays its `gasPrice`; everything above the base fee
    /// is the beneficiary's.
    #[test]
    fn a_legacy_transaction_pays_its_gas_price() {
        let (alice, bob, carol) = (
            Address::repeat_byte(0xAA),
            Address::repeat_byte(0xBB),
            Address::repeat_byte(0xCC),
        );
        let mut db = funded_db(&[(alice, ETH)]);
        let nodes = SharedMemNodeStore::new();
        let fees = FeeEnv {
            base_fee: 5,
            beneficiary: bob,
            charge: true,
        };
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            fees,
            transfer_tx(alice, carol, 0, 9, None),
        )
        .unwrap();

        let gas = u128::from(ARKIV_TX_GAS);
        assert_eq!(balance_in(&rs, alice), U256::from(ETH - gas * 9));
        assert_eq!(balance_in(&rs, bob), U256::from(gas * 4));
    }

    /// A zero tip credits nothing: the beneficiary must not be touched into
    /// existence as an empty account (which would change the state root).
    #[test]
    fn a_zero_tip_does_not_touch_the_beneficiary() {
        let (alice, bob, carol) = (
            Address::repeat_byte(0xAA),
            Address::repeat_byte(0xBB),
            Address::repeat_byte(0xCC),
        );
        let mut db = funded_db(&[(alice, ETH)]);
        let nodes = SharedMemNodeStore::new();
        let fees = FeeEnv {
            base_fee: 5,
            beneficiary: bob,
            charge: true,
        };
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            fees,
            transfer_tx(alice, carol, 0, 5, Some(0)),
        )
        .unwrap();

        assert_eq!(
            balance_in(&rs, alice),
            U256::from(ETH - u128::from(ARKIV_TX_GAS) * 5)
        );
        assert!(
            !rs.state.contains_key(&bob),
            "the beneficiary must not appear in the diff for a zero tip",
        );
    }

    /// Entity calls settle the same way: the metered gas at the effective price,
    /// with the tip credited.
    #[test]
    fn an_entity_call_is_priced_at_the_effective_price() {
        let (alice, bob) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xBB));
        let mut db = funded_db(&[(alice, ETH)]);
        let nodes = SharedMemNodeStore::new();
        let fees = FeeEnv {
            base_fee: 5,
            beneficiary: bob,
            charge: true,
        };
        let tx = TxEnv {
            tx_type: 2,
            gas_price: 10,
            gas_priority_fee: Some(3),
            ..arkiv_tx(alice, create_calldata(50, b"hello"))
        };
        let rs = arkiv_transact(&mut db, &nodes, 10, fees, tx).unwrap();
        assert!(rs.result.is_success());

        let gas = u128::from(rs.result.tx_gas_used());
        assert!(gas > 0);
        assert_eq!(balance_in(&rs, alice), U256::from(ETH - gas * 8));
        assert_eq!(balance_in(&rs, bob), U256::from(gas * 3));
    }

    /// A fee cap below the block base fee is rejected — as the typed error the
    /// payload builder skips on — unless the base-fee check is disabled, which
    /// reth does for `eth_call` / `eth_estimateGas`.
    #[test]
    fn a_fee_cap_below_the_base_fee_is_rejected_unless_disabled() {
        let (alice, carol) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xCC));
        let mut db = funded_db(&[(alice, ETH)]);
        let tx = transfer_tx(alice, carol, 0, 4, Some(4));

        let err = validate_fees(&mut db, &tx, 5, ALL_CHECKS).expect_err("4 < base fee 5");
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::GasPriceLessThanBasefee)
        ));

        // A legacy price below the base fee is rejected the same way.
        let legacy = transfer_tx(alice, carol, 0, 4, None);
        assert!(matches!(
            validate_fees(&mut db, &legacy, 5, ALL_CHECKS),
            Err(EVMError::Transaction(
                InvalidTransaction::GasPriceLessThanBasefee
            ))
        ));

        let relaxed = FeeChecks {
            base_fee: false,
            ..ALL_CHECKS
        };
        validate_fees(&mut db, &tx, 5, relaxed).expect("eth_call-style execution");
    }

    #[test]
    fn a_priority_fee_above_the_fee_cap_is_rejected() {
        let (alice, carol) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xCC));
        let mut db = funded_db(&[(alice, ETH)]);
        let tx = transfer_tx(alice, carol, 0, 10, Some(11));
        assert!(matches!(
            validate_fees(&mut db, &tx, 5, ALL_CHECKS),
            Err(EVMError::Transaction(
                InvalidTransaction::PriorityFeeGreaterThanMaxFee
            ))
        ));
    }

    /// A sender that cannot cover `gas_limit × max_fee + value` is rejected
    /// before anything is charged — typed, so a pooled transaction whose sender
    /// was drained since ingress is skipped rather than aborting the block. Before
    /// this check the debit saturated and the transaction went through anyway.
    #[test]
    fn an_underfunded_sender_is_rejected_as_invalid_not_fatal() {
        use alloy_evm::EvmError;

        let (alice, carol) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xCC));
        let gas = u128::from(ARKIV_TX_GAS);
        // One wei short of gas_limit × max_fee + value.
        let mut db = funded_db(&[(alice, gas * 10 + 100 - 1)]);
        let tx = transfer_tx(alice, carol, 100, 10, Some(3));

        let err = validate_fees(&mut db, &tx, 5, ALL_CHECKS).expect_err("one wei short");
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { .. })
        ));
        err.try_into_invalid_tx_err()
            .expect("must convert to an invalid-tx error, or reth aborts the block instead");

        // Exactly enough for the maximum spend passes, even though the effective
        // charge will be lower.
        let mut db = funded_db(&[(alice, gas * 10 + 100)]);
        validate_fees(&mut db, &tx, 5, ALL_CHECKS).expect("can afford the max spend");

        // And the check can be switched off (engine-tree payload prewarming).
        let mut db = funded_db(&[(alice, 0)]);
        let relaxed = FeeChecks {
            balance: false,
            ..ALL_CHECKS
        };
        validate_fees(&mut db, &tx, 5, relaxed).expect("balance check disabled");
    }

    /// With fee charging disabled (reth's `eth_call` / `eth_estimateGas`) the
    /// sender need not afford the gas — revm skips that bound — but must still
    /// cover the value, or the dry run would move value that does not exist.
    #[test]
    fn with_charging_disabled_only_the_value_must_be_covered() {
        let (alice, carol) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xCC));
        let dry_run = FeeChecks {
            charge: false,
            ..ALL_CHECKS
        };
        // Explicit gas and fee cap far beyond the sender's means, as a wallet's
        // eth_call may carry; the value itself is covered exactly.
        let tx = transfer_tx(alice, carol, 100, 1_000_000_000, Some(3));

        let mut db = funded_db(&[(alice, 100)]);

        validate_fees(&mut db, &tx, 5, dry_run).expect("gas is not charged, value is covered");
        assert!(
            validate_fees(&mut db, &tx, 5, ALL_CHECKS).is_err(),
            "the same sender cannot afford the gas once it is charged",
        );

        let mut db = funded_db(&[(alice, 99)]);

        let err = validate_fees(&mut db, &tx, 5, dry_run).expect_err("one wei short of the value");
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { fee, balance })
                if *fee == U256::from(100) && *balance == U256::from(99)
        ));
    }

    /// With charging disabled (reth's `eth_call` / `eth_estimateGas`) the value
    /// still moves and the nonce still bumps, but no gas is debited and nothing
    /// reaches the beneficiary.
    #[test]
    fn a_disabled_fee_charge_moves_value_only() {
        let (alice, bob, carol) = (
            Address::repeat_byte(0xAA),
            Address::repeat_byte(0xBB),
            Address::repeat_byte(0xCC),
        );
        let mut db = funded_db(&[(alice, ETH)]);
        let nodes = SharedMemNodeStore::new();
        let fees = FeeEnv {
            base_fee: 5,
            beneficiary: bob,
            charge: false,
        };
        let rs = arkiv_transact(
            &mut db,
            &nodes,
            10,
            fees,
            transfer_tx(alice, carol, 100, 10, Some(3)),
        )
        .unwrap();

        assert_eq!(balance_in(&rs, alice), U256::from(ETH - 100));
        assert_eq!(balance_in(&rs, carol), U256::from(100));
        assert_eq!(rs.state.get(&alice).unwrap().info.nonce, 1);
        assert!(!rs.state.contains_key(&bob));
    }
}
