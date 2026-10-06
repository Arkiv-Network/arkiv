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
//! one [`host_manager`] view (arkiv-reth-statemanager's `HostStateView` over
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
use arkiv_interfaces::gas::CostModel;
use arkiv_interfaces::primitives::{Hash, UserAddress, UserBalance};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, BlockRef, EntityCreationNoncesStore, EntityStore, EqualityIndexStore,
    RangeIndexStore, ReadMode, StateView,
};
use arkiv_reth_statemanager::{HostStore, host_manager};

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
    /// Whether [`validate_fees`] proved the sender could cover this transaction.
    ///
    /// On every path but engine-tree payload prewarming it did, which makes a
    /// clamped debit in [`charge_sender`] a broken invariant rather than a poor
    /// sender — see [`debit`].
    pub solvency_checked: bool,
}

impl FeeEnv {
    /// The fee inputs of `block`, with charging on.
    pub fn from_block(block: &BlockEnv) -> Self {
        Self {
            base_fee: block.basefee,
            beneficiary: block.beneficiary,
            charge: true,
            solvency_checked: true,
        }
    }

    /// What `tx` pays per gas in this block — revm's effective gas price:
    /// `min(max_fee, base_fee + priority_fee)`, or a legacy tx's `gasPrice`.
    fn effective_gas_price(&self, tx: &TxEnv) -> u128 {
        tx.effective_gas_price(u128::from(self.base_fee))
    }
}

/// Debit `amount` from `sender`, and refuse to let a clamp pass unnoticed.
///
/// [`UserBalance`] arithmetic saturates, because accounting must not fail in the
/// middle of a block once a transaction has been admitted. That is the right
/// contract for the primitive and the wrong place to decide solvency: by the time
/// a debit runs, [`validate_fees`] has already proved the sender covers
/// `gas_limit × max_fee + value` — or, with fee charging off, the value alone.
///
/// So whenever that check ran, `before < amount` cannot happen, and if it does the
/// bound and the debit have drifted apart. Saturating silently would destroy the
/// difference and leave nothing behind; this reports it instead, as the same typed
/// error the bound itself raises, so the payload builder skips the transaction
/// rather than aborting the block.
///
/// Engine-tree payload prewarming is the one path that disables the balance check.
/// There the clamp is intended and the result is cache-only, so it is allowed.
fn debit<V: StateView, DBError>(
    view: &mut V,
    fees: &FeeEnv,
    sender: UserAddress,
    amount: U256,
    context: &'static str,
) -> Result<(), EVMError<DBError>> {
    let before = view
        .fetch_sub_balance(sender, as_balance(amount))
        .map_err(state_fault(context))?;
    if fees.solvency_checked && before < as_balance(amount) {
        return Err(EVMError::Transaction(
            InvalidTransaction::LackOfFundForMaxFee {
                fee: Box::new(amount),
                balance: Box::new(U256::from_be_bytes(before.to_be_bytes())),
            },
        ));
    }
    Ok(())
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
fn charge_sender<V: StateView, DBError>(
    view: &mut V,
    fees: &FeeEnv,
    sender: Address,
    value_out: U256,
    gas_used: u64,
    tx: &TxEnv,
) -> Result<(), EVMError<DBError>> {
    let sender = sender.into_array();
    debit(view, fees, sender, value_out, "debit sender value")?;
    view.fetch_increment_acc_nonce(sender)
        .map_err(state_fault("bump sender nonce"))?;
    if !fees.charge {
        return Ok(());
    }

    let price = fees.effective_gas_price(tx);
    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(price));
    debit(view, fees, sender, gas_cost, "debit sender gas")?;

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
    /// Where Arkiv's own state lives. One erased handle, cloned per EVM.
    store: HostStore,
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
            solvency_checked: fee_checks.balance,
            ..FeeEnv::from_block(self.inner.block())
        };
        let chain_id = self.inner.chain_id();
        let store = self.store.clone();
        let db = self.inner.db_mut();
        let store = &store;
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
        arkiv_transact(store, db, block_number, fees, tx)
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
fn arkiv_transact<DB: Database>(
    store: &HostStore,
    db: &mut DB,
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
            return arkiv_entity_nonce_call(store, db, &tx, block_number, &fees);
        }
        if selector == IEntityRegistry::customAttributeNamesCall::SELECTOR {
            return arkiv_custom_attribute_names_call(store, db, &tx, block_number, &fees);
        }
        if selector == IEntityRegistry::attributeTypeIdCall::SELECTOR {
            return arkiv_attribute_type_id_call(store, db, &tx, block_number, &fees);
        }
        if selector == purgeExpiredCall::SELECTOR {
            return arkiv_purge_expired(store, db, block_number, &tx);
        }
        return arkiv_entity_transact(store, db, block_number, &fees, &tx);
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
    let mut view =
        host_manager(store, db, parent_ref(block_number)).map_err(state_fault("open view"))?;
    charge_sender(&mut view, &fees, tx.caller, value_out, gas_used, &tx)?;
    if to != tx.caller {
        view.fetch_add_balance(to.into_array(), as_balance(tx.value))
            .map_err(state_fault("credit recipient"))?;
    }
    StateView::commit(&mut view).map_err(state_fault("commit transfer"))?;
    let state = view
        .finish(fees.charge)
        .map_err(state_fault("seal transfer"))?;

    let result = ExecutionResult::Success {
        reason: SuccessReason::Stop,
        gas: ResultGas::default().with_total_gas_spent(gas_used),
        logs: Vec::new(),
        output: Output::Call(Bytes::new()),
    };

    Ok(ResultAndState::new(result, state))
}

fn arkiv_purge_expired<DB: Database>(
    store: &HostStore,
    db: &mut DB,
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
    let mut view =
        host_manager(store, db, parent_ref(block_number)).map_err(state_fault("open view"))?;
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
    let state = view.finish(true).map_err(state_fault("seal purge"))?;
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
fn arkiv_entity_transact<DB: Database>(
    store: &HostStore,
    db: &mut DB,
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
    let mut view =
        host_manager(store, db, parent_ref(block_number)).map_err(state_fault("open view"))?;
    let start_nonce = view
        .get_entity_creation_nonce(env.caller, ReadMode::ViewWithOverlay)
        .map_err(state_fault("read minting nonce"))?;
    let outcome = match decode_ops(&env, &tx.data, start_nonce) {
        Ok(ops) => {
            let costs = *view.cost_model();
            run_ops(&mut view, costs, &env, ops).map_err(|e| EVMError::Custom(e.to_string()))?
        }
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
    let evm_state = view
        .finish(fees.charge)
        .map_err(state_fault("seal entity batch"))?;

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
fn arkiv_view_call<DB: Database>(
    store: &HostStore,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
    answer: impl FnOnce(&mut DB) -> Result<Result<Vec<u8>, Vec<u8>>, EVMError<DB::Error>>,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    let output = answer(db)?;

    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    let mut view =
        host_manager(store, db, parent_ref(block_number)).map_err(state_fault("open view"))?;
    charge_sender(&mut view, fees, tx.caller, U256::ZERO, gas_used, tx)?;
    StateView::commit(&mut view).map_err(state_fault("commit view call"))?;
    let state = view.finish(false).map_err(state_fault("seal view call"))?;

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
fn view_entity<DB: Database>(
    store: &HostStore,
    db: &mut DB,
    key: B256,
    block_number: u64,
) -> Result<Option<arkiv_interfaces::entity::Entity>, EVMError<DB::Error>> {
    let view =
        host_manager(store, db, parent_ref(block_number)).map_err(state_fault("open view"))?;
    let entity = view
        .get_entity(key.0, ReadMode::ViewOnBase)
        .map_err(state_fault("read entity"))?;
    view.finish(false).map_err(state_fault("close read view"))?;
    Ok(entity)
}

/// `entityNonce(owner)`: the owner's entity-key minting nonce, as a `uint64`.
///
/// SDKs `eth_call` this before sending creates to predict the keys the batch
/// will mint (`derive_entity_address(chain_id, owner, nonce + i, salt)`), so it
/// reads the same system-account slot the execute path mints from.
fn arkiv_entity_nonce_call<DB: Database>(
    store: &HostStore,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(store, db, tx, block_number, fees, |db| {
        let call = match IEntityRegistry::entityNonceCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("entityNonce", e))),
        };
        let nonce = host_manager(store, db, parent_ref(block_number))
            .map_err(state_fault("open view"))?
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
fn arkiv_custom_attribute_names_call<DB: Database>(
    store: &HostStore,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(store, db, tx, block_number, fees, |db| {
        let call = match IEntityRegistry::customAttributeNamesCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("customAttributeNames", e))),
        };
        let names = match live_entity(
            view_entity(store, db, call.entityKey, block_number)?,
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
fn arkiv_attribute_type_id_call<DB: Database>(
    store: &HostStore,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(store, db, tx, block_number, fees, |db| {
        let call = match IEntityRegistry::attributeTypeIdCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("attributeTypeId", e))),
        };
        let wanted = strip_trailing_zeros(call.name.0.to_vec());
        let type_id = live_entity(
            view_entity(store, db, call.entityKey, block_number)?,
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
/// staged in the view the batch ran over.
struct Outcome {
    gas_used: u64,
    revert: Option<Bytes>,
    logs: Vec<Log>,
}

/// Run a decoded batch through the write view: on success, fold the staged
/// deltas into the index stores and advance the minting nonce per create; on a
/// revert nothing is staged.
///
/// Generic over the view, so the same batch logic runs on any `StateView`
/// backend. `costs` is passed rather than read off the view: a cost schedule is
/// a property of the chain, not of whatever happens to be storing state.
fn run_ops<V: StateView, C: CostModel>(
    view: &mut V,
    costs: C,
    env: &ExecEnv,
    ops: Vec<Op>,
) -> Result<Outcome, eyre::Report> {
    let create_count = ops
        .iter()
        .filter(|o| matches!(o, Op::Create { .. }))
        .count() as u64;

    let mut effects = Vec::new();
    let out = ArkivExecutor::with_cost(costs)
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
#[derive(Debug, Clone)]
pub struct ArkivEvmFactory {
    store: HostStore,
}

impl ArkivEvmFactory {
    /// Build a factory over the store Arkiv's state lives in.
    pub const fn new(store: HostStore) -> Self {
        Self { store }
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
            store: self.store.clone(),
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
            store: self.store.clone(),
        }
    }
}

/// Builds the Arkiv block executor: reth's stock Ethereum block executor driven
/// by [`ArkivEvmFactory`]. This is the type arkiv-reth hands to
/// `EthereumNode::components().executor(..)`.
///
/// Generic over the node's chain spec with the same bounds reth's own
/// `EthereumExecutorBuilder` asks for, so the node can run on arkiv-reth-chainspec's
/// `ArkivChainSpec` (the minimum-base-fee rule) as well as on reth's `ChainSpec`.
#[derive(Debug, Clone)]
pub struct ArkivExecutorBuilder {
    store: HostStore,
}

impl ArkivExecutorBuilder {
    /// Build the executor over the store Arkiv's state lives in.
    pub const fn new(store: HostStore) -> Self {
        Self { store }
    }
}

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
        Ok(EthEvmConfig::new_with_evm_factory(
            ctx.chain_spec(),
            ArkivEvmFactory::new(self.store.clone()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAMP_ALICE: UserAddress = [0xaa; 20];
    const CLAMP_GENESIS: BlockRef = BlockRef {
        height: 0,
        hash: [0; 32],
    };

    #[allow(clippy::arc_with_non_send_sync)] // MemStore is a RefCell; see its docs.
    fn clamp_view(
        amount: u64,
    ) -> arkiv_golemdb_state::GolemStateView<
        std::sync::Arc<arkiv_interfaces::store::reference::MemStore>,
    > {
        use arkiv_interfaces::statemanager::StateManager;
        use arkiv_interfaces::store::{Store, StoreExt};

        let store = std::sync::Arc::new(arkiv_interfaces::store::reference::MemStore::new());
        let branch = store.begin(None).expect("begin");
        store
            .commit_tagged(branch, CLAMP_GENESIS.hash)
            .expect("tag genesis");
        let mut view = arkiv_golemdb_state::GolemStateManager::new(store)
            .view(CLAMP_GENESIS)
            .expect("view");
        view.fetch_add_balance(CLAMP_ALICE, UserBalance::from_u64(amount))
            .expect("fund");
        view
    }

    fn clamp_fees(solvency_checked: bool) -> FeeEnv {
        FeeEnv {
            base_fee: 0,
            beneficiary: Address::ZERO,
            charge: true,
            solvency_checked,
        }
    }

    /// `validate_fees` has already proved the sender covers this, so a clamp means
    /// the bound and the debit disagree. Saturating would destroy the difference
    /// silently; the drift has to surface.
    #[test]
    fn a_clamped_debit_is_reported_when_solvency_was_checked() {
        let mut view = clamp_view(10);
        let outcome = debit::<_, core::convert::Infallible>(
            &mut view,
            &clamp_fees(true),
            CLAMP_ALICE,
            U256::from(11),
            "test",
        );
        assert!(
            matches!(
                outcome,
                Err(EVMError::Transaction(
                    InvalidTransaction::LackOfFundForMaxFee { .. }
                ))
            ),
            "a clamped debit passed unnoticed: {outcome:?}"
        );
    }

    /// Engine-tree payload prewarming disables the balance check deliberately and
    /// discards what it computes, so clamping there is intended.
    #[test]
    fn a_clamped_debit_is_allowed_when_solvency_was_not_checked() {
        let mut view = clamp_view(10);
        debit::<_, core::convert::Infallible>(
            &mut view,
            &clamp_fees(false),
            CLAMP_ALICE,
            U256::from(11),
            "test",
        )
        .expect("prewarming may clamp");
        assert_eq!(
            view.get_balance(CLAMP_ALICE, ReadMode::ViewWithOverlay)
                .unwrap(),
            UserBalance::ZERO
        );
    }

    /// The ordinary path still just moves money.
    #[test]
    fn a_covered_debit_passes() {
        let mut view = clamp_view(10);
        debit::<_, core::convert::Infallible>(
            &mut view,
            &clamp_fees(true),
            CLAMP_ALICE,
            U256::from(4),
            "test",
        )
        .expect("covered");
        assert_eq!(
            view.get_balance(CLAMP_ALICE, ReadMode::ViewWithOverlay)
                .unwrap(),
            UserBalance::from_u64(6)
        );
    }
}
