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
//! never charged. One exception to the burn: the base-fee share of an entity
//! transaction's storage gas is credited to the pool, the account at
//! [`ARKIV_ADDRESS`] ([`credit_pool`]), together with any value a successful call
//! there was sent.
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
use arkiv_interfaces::gas::{CostModel, GasBreakdown, OpCost};
use arkiv_interfaces::primitives::{Hash, UserAddress, UserBalance};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, AccountNoncesStore, BlockRef, EntityCreationNoncesStore, EntityStore,
    EqualityIndexStore, RangeIndexStore, ReadMode, StateView,
};
use arkiv_interfaces::store::{BranchId, CommitId, Origin};
use arkiv_reth_statemanager::{
    BlockSeals, ExecutionKey, HostStateView, HostStore, OwnedBranch, WriteOverlay, host_manager,
};
use arkiv_tokenomics::FeeSchedule;
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
    tx_gas(data).max(floor_gas(data))
}

/// The calldata's tokens, as EIP-2028 and EIP-7623 count them: a zero byte is
/// one, any other byte four.
fn calldata_tokens(data: &[u8]) -> u64 {
    let zero = data.iter().filter(|b| **b == 0).count() as u64;
    let nonzero = data.len() as u64 - zero;
    zero + 4 * nonzero
}

/// A transaction's own terms: 21,000 plus its calldata (4 per token). An
/// entity transaction adds them to its compute rather than taking the larger
/// of them and the ops.
fn tx_gas(data: &[u8]) -> u64 {
    ARKIV_TX_GAS + 4 * calldata_tokens(data)
}

/// The least a transaction pays for its calldata (EIP-7623: 21,000 plus 10
/// per token), applied once to an entity transaction's compute.
fn floor_gas(data: &[u8]) -> u64 {
    ARKIV_TX_GAS + 10 * calldata_tokens(data)
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

/// The parent-height ref a write view reports as its base. reth owns
/// canonicality on this host, so the hash is not threaded through and stays
/// zero — and the branch itself is always over head regardless, so this is a
/// label on the view rather than a choice of base.
fn parent_ref(block_number: u64) -> BlockRef {
    BlockRef::new(block_number.saturating_sub(1), Hash::default())
}

/// Open the write view a transaction stages into.
///
/// A speculative run -- fee charging off: `eth_call`, `eth_estimateGas` -- also
/// opens a frame, which [`close_view`] rolls back. The view writes through to the
/// block's branch, and reth runs every probe of an estimate on one EVM, hence one
/// branch, expecting `transact` to leave state alone. Without the frame, a delete
/// estimated at the cap removes the entity, every lower probe then reverts with
/// `EntityNotFound`, and the estimate comes back as the cap.
fn open_view<'a, DB: Database>(
    store: &HostStore,
    branch: BranchId,
    db: &'a mut DB,
    block_number: u64,
    fees: &FeeEnv,
) -> Result<HostStateView<WriteOverlay<'a, DB>>, EVMError<DB::Error>> {
    let mut view = host_manager(store, branch, db, parent_ref(block_number))
        .map_err(state_fault("open view"))?;
    if !fees.charge {
        view.golem_mut()
            .begin_frame()
            .map_err(state_fault("open speculative frame"))?;
    }
    Ok(view)
}

/// Commit what `view` staged and hand back reth's account diff -- then, for a
/// speculative run, roll the branch back to where [`open_view`] found it.
fn close_view<DB: Database>(
    mut view: HostStateView<WriteOverlay<'_, DB>>,
    fees: &FeeEnv,
    context: &'static str,
) -> Result<EvmState, EVMError<DB::Error>> {
    StateView::commit(&mut view).map_err(state_fault(context))?;
    if !fees.charge {
        view.golem_mut()
            .discard_frame()
            .map_err(state_fault("discard speculative frame"))?;
    }
    view.finish().map_err(state_fault("close view"))
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
/// With [`FeeEnv::charge`] off only the value moves, and a zero value not at
/// all. Only gas actually used is charged; there is no up-front `gas_limit`
/// debit and refund because nothing runs between the two.
fn charge_sender<V: StateView, DBError>(
    view: &mut V,
    fees: &FeeEnv,
    sender: Address,
    value_out: U256,
    gas_used: u64,
    tx: &TxEnv,
) -> Result<(), EVMError<DBError>> {
    let sender = sender.into_array();
    if !fees.charge {
        // A question, not a transaction: reth turns fee charging off for
        // `eth_call` and `eth_estimateGas`. revm bumps the nonce anyway, on a
        // state it is about to throw away -- but this view writes through to
        // the store's branch, and that branch is the block's. Bumping here
        // would let an estimate advance the sender's nonce for real, and the
        // pool would then reject the very transaction being estimated for.
        // A zero debit would write all the same, creating an account for a
        // caller that has none, so a probe moves only a value there is.
        if !value_out.is_zero() {
            debit(view, fees, sender, value_out, "debit sender value")?;
        }
        return Ok(());
    }
    debit(view, fees, sender, value_out, "debit sender value")?;
    view.fetch_increment_acc_nonce(sender)
        .map_err(state_fault("bump sender nonce"))?;

    // `u64` gas times a `u128` price always fits `U256`. Checked all the same,
    // so a change to either width fails loudly rather than capping a charge.
    let overflow = || EVMError::Custom("a gas charge does not fit U256".into());
    let price = fees.effective_gas_price(tx);
    let gas_cost = U256::from(gas_used)
        .checked_mul(U256::from(price))
        .ok_or_else(overflow)?;
    debit(view, fees, sender, gas_cost, "debit sender gas")?;

    let tip = price.saturating_sub(u128::from(fees.base_fee));
    if tip > 0 {
        let reward = U256::from(gas_used)
            .checked_mul(U256::from(tip))
            .ok_or_else(overflow)?;
        view.fetch_add_balance(fees.beneficiary.into_array(), as_balance(reward))
            .map_err(state_fault("credit beneficiary tip"))?;
    }
    Ok(())
}

/// The value a call to [`ARKIV_ADDRESS`] moves to the pool: all of `tx.value`
/// when it succeeded, none when it reverted, as a revert undoes a value
/// transfer in the EVM. A probe moves none either: like its nonce, a moved
/// value would land on the block's branch.
fn value_in(fees: &FeeEnv, succeeded: bool, tx: &TxEnv) -> U256 {
    if fees.charge && succeeded {
        tx.value
    } else {
        U256::ZERO
    }
}

/// Credit the pool, the account at [`ARKIV_ADDRESS`], after [`charge_sender`]
/// settled the same transaction: `value_in`, the value a successful call was
/// sent (a payable contract keeps what it is sent), and `base_fee ×
/// storage_gas`, the base-fee share of the storage bought, which is then not
/// burned. The sender paid both already, the value as `value_out` and the
/// share inside the gas charge.
///
/// With [`FeeEnv::charge`] off nothing moves, so a probe leaves the pool as it
/// was. A zero credit touches nothing, so the pool comes into existence with
/// its first credit, not with the first call to its address.
fn credit_pool<V: StateView, DBError>(
    view: &mut V,
    fees: &FeeEnv,
    value_in: U256,
    storage_gas: u64,
) -> Result<(), EVMError<DBError>> {
    if !fees.charge {
        return Ok(());
    }
    // `u64` gas times a `u64` base fee always fits `U256`; the value was
    // debited from a balance, so it fits as well. The sum saturates like the
    // balance it is added to, as every credit here does (recipient, tip). That
    // never loses money while the total supply fits `U256`: nothing mints
    // after genesis, so the genesis allocations bound every balance.
    let storage_fee = U256::from(storage_gas) * U256::from(fees.base_fee);
    let credit = value_in.saturating_add(storage_fee);
    if !credit.is_zero() {
        view.fetch_add_balance(ARKIV_ADDRESS.into_array(), as_balance(credit))
            .map_err(state_fault("credit pool"))?;
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
    /// The sealed candidates, shared with the node that adopts one of them.
    seals: Arc<BlockSeals>,
    /// This block's branch, opened by the first transaction. Every transaction
    /// in the block runs on it, in order, so each sees the last one's writes.
    branch: Option<OwnedBranch>,
    /// Captured once for this execution, including eth_call/estimate probes.
    pricing: Result<BlockPricing, String>,
    /// What an op costs beyond the store's receipts, by the kind of store.
    op_cost: OpCost,
    /// Whether any charged transaction ran. A speculative execution —
    /// `eth_call`, `eth_estimateGas` — never produces a block, so its branch
    /// is discarded rather than sealed.
    charged: core::cell::Cell<bool>,
    /// `(signer, nonce)` of each charged transaction, in order — the half of
    /// [`ExecutionKey`] only the EVM sees.
    ///
    /// Recorded because height does not identify an execution: two builds at
    /// one height race on separate threads, and the payload builder has to be
    /// told the root of *its* execution, not of whichever sealed last.
    executed: core::cell::RefCell<Vec<(UserAddress, u64)>>,
}

impl<DB: Database, I> ArkivEvm<DB, I> {
    fn new(
        inner: EthEvm<DB, I, PrecompilesMap>,
        store: HostStore,
        seals: Arc<BlockSeals>,
        pricing: Result<BlockPricing, String>,
        op_cost: OpCost,
    ) -> Self {
        Self {
            inner,
            store,
            seals,
            branch: None,
            pricing,
            op_cost,
            charged: core::cell::Cell::new(false),
            executed: core::cell::RefCell::new(Vec::new()),
        }
    }

    /// The block's branch, opened on first use over `base`, the commit its
    /// prices came from.
    fn branch(&mut self, base: CommitId) -> Result<BranchId, EVMError<DB::Error>> {
        if let Some(branch) = &self.branch {
            return Ok(branch.id());
        }
        let branch =
            OwnedBranch::begin(&self.store).map_err(state_fault("open the block's branch"))?;
        let origin = self
            .store
            .branch_info(branch.id())
            .map_err(state_fault("read the branch's base"))?
            .origin;
        // The head moved since the prices were read. Dropping the branch discards it.
        if origin != Origin::Commit(base) {
            return Err(EVMError::Custom(
                "pricing base is no longer the store head".into(),
            ));
        }
        Ok(self.branch.insert(branch).id())
    }
}

/// The block's price list, read once at its base and kept for its whole life.
#[derive(Debug, Clone)]
struct BlockPricing {
    /// The commit the list was read at: the block's base.
    commit_id: CommitId,
    schedule: FeeSchedule,
}

impl BlockPricing {
    fn load(store: &HostStore, at: CommitId) -> Result<Self, String> {
        let schedule = match store.pricing_snapshot(at) {
            Ok(snapshot) if snapshot.commit_id != at => {
                return Err("pricing snapshot does not match the block base".into());
            }
            Ok(snapshot) => FeeSchedule::from_model(&snapshot.model)
                .map_err(|e| format!("host pricing: {e:?}"))?,
            // The in-memory mock keeps no price list.
            Err(arkiv_interfaces::store::StoreError::PricingUnavailable) => FeeSchedule::MOCK,
            Err(e) => return Err(format!("pricing snapshot: {e:?}")),
        };
        Ok(Self {
            commit_id: at,
            schedule,
        })
    }
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
        let pricing = self
            .pricing
            .as_ref()
            .map_err(|e| EVMError::Custom(e.clone()))?;
        let schedule = pricing.schedule;
        let base = pricing.commit_id;
        // Advancing head invalidates the branch too; never refresh its prices.
        if self.store.head() != base {
            return Err(EVMError::Custom(
                "pricing base is no longer the store head".into(),
            ));
        }
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
        let sender = (tx.caller.into_array(), tx.nonce);
        let store = self.store.clone();
        let branch = self.branch(base)?;
        let db = self.inner.db_mut();
        let store = &store;
        let has_purge_selector = tx.kind == TxKind::Call(ARKIV_ADDRESS)
            && tx.data.starts_with(&purgeExpiredCall::SELECTOR);
        let is_protocol_purge = is_protocol_purge(&tx, block_number, chain_id);
        if has_purge_selector && !is_protocol_purge {
            return Err(invalid_transaction("purgeExpired is protocol-only"));
        }
        let checks = (!is_protocol_purge).then_some(SenderChecks {
            nonce: nonce_check,
            fees: fee_checks,
        });
        let op_cost = self.op_cost;
        let result = arkiv_transact(
            store,
            branch,
            &schedule,
            op_cost,
            db,
            block_number,
            fees,
            checks,
            tx,
        )?;
        // Only charged transactions, in arrival order, and only once executed:
        // this is the list reth will hand back as the block's transactions, so
        // the two have to agree exactly or the built header's root goes
        // unanswered. One refused here is skipped by the builder, so it is not
        // in the block either.
        if fees.charge {
            self.charged.set(true);
            self.executed.borrow_mut().push(sender);
        }
        Ok(result)
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

    /// Block execution is over. Seal the branch as a candidate at this height
    /// if a real block ran on it; otherwise it is dropped, and discarded.
    ///
    /// Sealing computes the roots and writes nothing, so reth can execute this
    /// block again — to validate what it just built — and get the same answer.
    /// Only adoption commits.
    fn finish(self) -> (DB, EvmEnv<SpecId, BlockEnv>) {
        let height = self.inner.block().number.saturating_to::<u64>();
        if self.charged.get()
            && let Some(branch) = self.branch
        {
            let key = ExecutionKey::new(height, self.executed.into_inner());
            if let Err(error) = self.seals.seal(key, branch) {
                tracing::error!(target: "arkiv::executor", ?error, height, "failed to seal the block's Arkiv state");
            }
        }
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
/// The sender's nonce and balance checks, read from the Arkiv view.
///
/// Not from reth's `db`: this node no longer writes reth's database -- its
/// persistence service is ours and commits to the store instead -- so `db`
/// answers with the genesis allocation for every account, forever. Execution
/// has always read the store; only these two checks did not, and they agreed
/// with it for as long as reth kept a second copy (#178).
///
/// Takes the view the caller already opened rather than opening its own. A
/// second view is a second frame on the branch, and a frame is what a reverted
/// call rolls back to -- so opening one turns a reverted call into a state
/// change.
fn check_sender<V, E, DBE>(
    view: &V,
    checks: Option<SenderChecks>,
    fees: &FeeEnv,
    tx: &TxEnv,
) -> Result<(), EVMError<DBE>>
where
    V: AccountBalancesStore<Error = E> + AccountNoncesStore<Error = E>,
    E: core::fmt::Debug,
{
    let Some(checks) = checks else { return Ok(()) };
    let caller = tx.caller.into_array();
    if checks.nonce {
        let nonce = view
            .get_acc_nonce(caller, ReadMode::ViewWithOverlay)
            .map_err(state_fault("read the sender's nonce"))?;
        validate_nonce(nonce.get(), tx)?;
    }
    let balance = view
        .get_balance(caller, ReadMode::ViewWithOverlay)
        .map_err(state_fault("read the sender's balance"))?;
    validate_fees(
        U256::from_be_bytes(balance.to_be_bytes()),
        tx,
        fees.base_fee,
        checks.fees,
    )
}

/// Which sender pre-checks a transaction gets, and on what terms.
///
/// `None` means a protocol purge, which is not a user transaction and is
/// subject to neither.
#[derive(Debug, Clone, Copy)]
struct SenderChecks {
    /// Whether the nonce must match.
    nonce: bool,
    /// Which fee checks apply, which the RPC path varies.
    fees: FeeChecks,
}

fn validate_nonce<DBE>(state: u64, tx: &TxEnv) -> Result<(), EVMError<DBE>> {
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
fn validate_fees<DBE>(
    balance: U256,
    tx: &TxEnv,
    base_fee: u64,
    checks: FeeChecks,
) -> Result<(), EVMError<DBE>> {
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
#[allow(clippy::too_many_arguments)] // Explicit block pricing alongside state, fees and sender checks.
fn arkiv_transact<DB: Database>(
    store: &HostStore,
    branch: BranchId,
    schedule: &FeeSchedule,
    op_cost: OpCost,
    db: &mut DB,
    block_number: u64,
    fees: FeeEnv,
    checks: Option<SenderChecks>,
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
            return arkiv_entity_nonce_call(store, branch, db, &tx, block_number, &fees, checks);
        }
        if selector == IEntityRegistry::customAttributeNamesCall::SELECTOR {
            return arkiv_custom_attribute_names_call(
                store,
                branch,
                db,
                &tx,
                block_number,
                &fees,
                checks,
            );
        }
        if selector == IEntityRegistry::attributeTypeIdCall::SELECTOR {
            return arkiv_attribute_type_id_call(
                store,
                branch,
                db,
                &tx,
                block_number,
                &fees,
                checks,
            );
        }
        if selector == purgeExpiredCall::SELECTOR {
            return arkiv_purge_expired(store, branch, db, block_number, &tx);
        }
        return arkiv_entity_transact(
            store,
            branch,
            schedule,
            op_cost,
            db,
            block_number,
            &fees,
            checks,
            &tx,
        );
    }

    // Otherwise it's a plain value transfer. 21k flat, floored at the calldata
    // intrinsics should the transfer carry data. A limit below that is
    // invalid, before any write.
    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    if tx.gas_limit < gas_used {
        return Err(gas_limit_below(gas_used, &tx));
    }
    let value_out = if to == tx.caller {
        U256::ZERO
    } else {
        tx.value
    };

    // Sender debit + nonce bump and recipient credit, through one view — a
    // transfer stages the same way every other state change does.
    let mut view = open_view(store, branch, db, block_number, &fees)?;
    check_sender(&view, checks, &fees, &tx)?;
    charge_sender(&mut view, &fees, tx.caller, value_out, gas_used, &tx)?;
    if to != tx.caller {
        view.fetch_add_balance(to.into_array(), as_balance(tx.value))
            .map_err(state_fault("credit recipient"))?;
    }
    let state = close_view(view, &fees, "commit transfer")?;

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
    branch: BranchId,
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
    let mut view = host_manager(store, branch, db, parent_ref(block_number))
        .map_err(state_fault("open view"))?;
    let mut modeled_gas = 0u64;
    for key in &purge_keys {
        let key_bytes = key.0;
        let Some(entity) = view
            .get_entity(key_bytes, ReadMode::ViewWithOverlay)
            .map_err(state_fault("read purge entity"))?
            .into_value()
        else {
            continue;
        };
        if entity.expires_at > block_number {
            continue;
        }
        modeled_gas = modeled_gas.saturating_add(arkiv_interfaces::gas::purge_cost(
            entity.indexed_attribute_count(),
        ));
        if modeled_gas > tx.gas_limit {
            return Ok(out_of_gas());
        }
        view.update_entity(EntityUpdates::deletion(key_bytes))
            .map_err(state_fault("stage purge deletion"))?
            .into_value();
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
    let state = view.finish().map_err(state_fault("close view"))?;
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

/// A gas limit below what the transaction pays at least is invalid, as in
/// Ethereum. Halting instead would put it in a block with no gas, fee or nonce
/// bump, so the same signed transaction could be included again and again.
fn gas_limit_below<E>(initial_gas: u64, tx: &TxEnv) -> EVMError<E> {
    EVMError::Transaction(InvalidTransaction::CallGasCostMoreThanGasLimit {
        initial_gas,
        gas_limit: tx.gas_limit,
    })
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
#[allow(clippy::too_many_arguments)] // Same execution context as arkiv_transact.
fn arkiv_entity_transact<DB: Database>(
    store: &HostStore,
    branch: BranchId,
    schedule: &FeeSchedule,
    op_cost: OpCost,
    db: &mut DB,
    block_number: u64,
    fees: &FeeEnv,
    checks: Option<SenderChecks>,
    tx: &TxEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    // A limit below the intrinsic minimum is invalid, before anything is
    // staged. `eth_estimateGas` reads that as "gas too low" and raises the
    // lower bound of its search, so it never converges below the minimum.
    let floor = intrinsic_gas(&tx.data);
    if tx.gas_limit < floor {
        return Err(gas_limit_below(floor, tx));
    }

    let caller = tx.caller;

    // One view for the whole transaction: the entity phase and the sender
    // phase stage into the same overlay; a revert stages no entity changes, so
    // the one commit at the end flushes exactly what should land.
    // Not `open_view`: frames do not nest, and the executor opens the
    // transaction's own frame, which a speculative run discards
    // (`ExecEnv::keep_writes`). Nothing else here writes in one: the sender
    // phase moves nothing with fee charging off.
    let mut view = host_manager(store, branch, db, parent_ref(block_number))
        .map_err(state_fault("open view"))?;
    check_sender(&view, checks, fees, tx)?;
    // The keys the batch creates are derived from this nonce. Reading it is
    // entity cost, so its receipt starts the batch's meter.
    let start_nonce = view
        .get_entity_creation_nonce(caller.into_array(), ReadMode::ViewWithOverlay)
        .map_err(state_fault("read minting nonce"))?;
    let env = ExecEnv {
        caller: caller.into_array(),
        block_number,
        gas_supplied: tx.gas_limit,
        tx_gas: tx_gas(&tx.data),
        floor_gas: floor_gas(&tx.data),
        golemdb_cost_before: start_nonce.receipt.cost.0,
        chain_id: tx.chain_id.unwrap_or(1),
        max_lifetime: schedule.max_lifetime,
        // `eth_call` and `eth_estimateGas` run with charging off on the
        // block's branch, one probe after another: their writes must not
        // outlive the probe.
        keep_writes: fees.charge,
    };
    let outcome = match decode_ops(&env, &tx.data, start_nonce.value) {
        Ok(ops) => run_ops(&mut view, op_cost, schedule, &env, ops)
            .map_err(|e| EVMError::Custom(e.to_string()))?,
        // Calldata that does not decode runs no op: it pays its own terms and
        // the nonce read before it, at least the floor, at most the gas
        // supplied. A sum that does not fit is out of gas, as everywhere.
        Err(e) => {
            let (compute_gas, revert) = match env.tx_gas.checked_add(env.golemdb_cost_before) {
                Some(compute) => (
                    compute.max(env.floor_gas).min(env.gas_supplied),
                    revert::decode_revert_data(&e),
                ),
                None => (
                    env.gas_supplied,
                    revert::revert_data(&arkiv_interfaces::RevertReason::OutOfGas),
                ),
            };
            Outcome {
                gas: GasBreakdown {
                    compute_gas,
                    storage_gas: 0,
                    purge_gas: 0,
                },
                revert: Some(revert),
                logs: Vec::new(),
            }
        }
    };

    // Sender phase: charge gas, bump the EOA nonce. The batch's breakdown
    // already holds the transaction's terms and the floor, applied once, so a
    // cheap batch still estimates to a pool-acceptable gas limit. Then the
    // pool: the value of a successful batch and its storage share.
    let gas_used = outcome
        .gas
        .total()
        .ok_or_else(|| EVMError::Custom("the transaction's gas does not fit u64".into()))?;
    let value_in = value_in(fees, outcome.revert.is_none(), tx);
    charge_sender(&mut view, fees, caller, value_in, gas_used, tx)?;
    credit_pool(&mut view, fees, value_in, outcome.gas.storage_gas)?;
    StateView::commit(&mut view).map_err(state_fault("commit transaction"))?;
    let evm_state = view.finish().map_err(state_fault("close view"))?;

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
/// The only state staged is the sender's flat gas charge + EOA nonce bump, and
/// the value of an answered call to the pool: meaningless for an `eth_call`
/// (the diff is discarded), but it keeps reth's sender invariants intact if the
/// call ever arrives as a mined transaction. So the sender is checked like any
/// transaction's, before the first write: the view writes through to the
/// block's branch, and an error after a write would leave the write behind.
#[allow(clippy::too_many_arguments)]
fn arkiv_view_call<DB: Database>(
    store: &HostStore,
    branch: BranchId,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
    checks: Option<SenderChecks>,
    answer: impl FnOnce(&mut DB) -> Result<Result<Vec<u8>, Vec<u8>>, EVMError<DB::Error>>,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    // A limit below the flat charge is invalid, before any write.
    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    if tx.gas_limit < gas_used {
        return Err(gas_limit_below(gas_used, tx));
    }
    let output = answer(db)?;

    let mut view = open_view(store, branch, db, block_number, fees)?;
    check_sender(&view, checks, fees, tx)?;
    let value_in = value_in(fees, output.is_ok(), tx);
    charge_sender(&mut view, fees, tx.caller, value_in, gas_used, tx)?;
    credit_pool(&mut view, fees, value_in, 0)?;
    let state = close_view(view, fees, "commit view call")?;

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
    branch: BranchId,
    db: &mut DB,
    key: B256,
    block_number: u64,
) -> Result<Option<arkiv_interfaces::entity::Entity>, EVMError<DB::Error>> {
    let view = host_manager(store, branch, db, parent_ref(block_number))
        .map_err(state_fault("open view"))?;
    let entity = view
        .get_entity(key.0, ReadMode::ViewOnBase)
        .map_err(state_fault("read entity"))?
        .into_value();
    view.finish().map_err(state_fault("close view"))?;
    Ok(entity)
}

/// `entityNonce(owner)`: the owner's entity-key minting nonce, as a `uint64`.
///
/// SDKs `eth_call` this before sending creates to predict the keys the batch
/// will mint (`derive_entity_address(chain_id, owner, nonce + i, salt)`), so it
/// reads the same system-account slot the execute path mints from.
fn arkiv_entity_nonce_call<DB: Database>(
    store: &HostStore,
    branch: BranchId,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
    checks: Option<SenderChecks>,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(store, branch, db, tx, block_number, fees, checks, |db| {
        let call = match IEntityRegistry::entityNonceCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("entityNonce", e))),
        };
        let nonce = host_manager(store, branch, db, parent_ref(block_number))
            .map_err(state_fault("open view"))?
            .get_entity_creation_nonce(call.owner.into_array(), ReadMode::ViewOnBase)
            .map_err(state_fault("read minting nonce"))?
            .into_value();
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
    branch: BranchId,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
    checks: Option<SenderChecks>,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(store, branch, db, tx, block_number, fees, checks, |db| {
        let call = match IEntityRegistry::customAttributeNamesCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("customAttributeNames", e))),
        };
        let names = match live_entity(
            view_entity(store, branch, db, call.entityKey, block_number)?,
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

/// `attributeTypeId(entityKey, name)`: the wire `typeId` that attribute holds —
/// indexed flag in bit 7, as a client writes it — or **0** if it is not set.
///
/// 0 is [`TOMBSTONE_TYPE_ID`](arkiv_interfaces::entity::TOMBSTONE_TYPE_ID) — the
/// tag that means "unset" on the wire — so "absent" reads the same here as it
/// does in a patch. No type ever has id 0, so the answer stays unambiguous.
fn arkiv_attribute_type_id_call<DB: Database>(
    store: &HostStore,
    branch: BranchId,
    db: &mut DB,
    tx: &TxEnv,
    block_number: u64,
    fees: &FeeEnv,
    checks: Option<SenderChecks>,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    arkiv_view_call(store, branch, db, tx, block_number, fees, checks, |db| {
        let call = match IEntityRegistry::attributeTypeIdCall::abi_decode_raw(&tx.data[4..]) {
            Ok(c) => c,
            Err(e) => return Ok(Err(bad_view_args("attributeTypeId", e))),
        };
        let wanted = strip_trailing_zeros(call.name.0.to_vec());
        let type_id = live_entity(
            view_entity(store, branch, db, call.entityKey, block_number)?,
            block_number,
        )
        .and_then(|e| {
            e.attributes.iter().find(|a| a.key == wanted).map(|a| {
                let flag = if a.indexed {
                    arkiv_interfaces::entity::INDEXED_FLAG
                } else {
                    0
                };
                a.value.type_id() | flag
            })
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
    gas: GasBreakdown,
    revert: Option<Bytes>,
    logs: Vec<Log>,
}

/// Run a decoded batch through the write view: on success, fold the staged
/// deltas into the index stores; on a revert nothing is staged. Each create
/// advances the minting nonce itself, inside the batch's frame.
///
/// Generic over the view, so the same batch logic runs on any `StateView`
/// backend. `costs` is passed rather than read off the view: a cost schedule is
/// a property of the chain, not of whatever happens to be storing state.
fn run_ops<V: StateView, C: CostModel>(
    view: &mut V,
    costs: C,
    schedule: &FeeSchedule,
    env: &ExecEnv,
    ops: Vec<Op>,
) -> Result<Outcome, eyre::Report> {
    let mut effects = Vec::new();
    let out = ArkivExecutor::with_cost(costs, *schedule)
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
            let logs = effects.iter().map(entity_operation_log).collect();
            Ok(Outcome {
                gas: out.gas,
                revert: None,
                logs,
            })
        }
        ExecStatus::Reverted => Ok(Outcome {
            gas: out.gas,
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
    seals: Arc<BlockSeals>,
    op_cost: OpCost,
}

impl ArkivEvmFactory {
    /// Build a factory over the store Arkiv's state lives in, parking each
    /// block's sealed state in `seals` until one is adopted. `op_cost` is
    /// what an op costs beyond that store's receipts.
    pub const fn new(store: HostStore, seals: Arc<BlockSeals>, op_cost: OpCost) -> Self {
        Self {
            store,
            seals,
            op_cost,
        }
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

        ArkivEvm::new(
            EthEvm::new(inner, false),
            self.store.clone(),
            self.seals.clone(),
            BlockPricing::load(&self.store, self.store.head()),
            self.op_cost,
        )
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let base = self.create_evm(db, input);
        let inner = base.inner.into_inner().with_inspector(inspector);
        ArkivEvm::new(
            EthEvm::new(inner, true),
            self.store.clone(),
            self.seals.clone(),
            base.pricing,
            self.op_cost,
        )
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
    seals: Arc<BlockSeals>,
    op_cost: OpCost,
}

impl ArkivExecutorBuilder {
    /// Build the executor over the store Arkiv's state lives in, with what an
    /// op costs beyond that store's receipts.
    pub const fn new(store: HostStore, seals: Arc<BlockSeals>, op_cost: OpCost) -> Self {
        Self {
            store,
            seals,
            op_cost,
        }
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
            ArkivEvmFactory::new(self.store.clone(), self.seals.clone(), self.op_cost),
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

    /// A probe (`eth_call`, `eth_estimateGas`) from a caller with no account
    /// leaves the branch exactly as it was: no zero-balance account appears.
    #[test]
    #[allow(clippy::arc_with_non_send_sync)] // MemStore is a RefCell; see its docs.
    fn a_probe_writes_no_account() {
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
        let before = view.digest().unwrap();

        let probe = FeeEnv {
            charge: false,
            ..clamp_fees(false)
        };
        charge_sender::<_, core::convert::Infallible>(
            &mut view,
            &probe,
            Address::repeat_byte(0xbb),
            U256::ZERO,
            50_000,
            &TxEnv::default(),
        )
        .expect("a probe settles");
        assert_eq!(view.digest().unwrap(), before, "the branch is untouched");
    }

    /// What one entity transaction through the whole entry left behind.
    struct Settled {
        success: bool,
        gas_used: u64,
        sender_paid: U256,
        beneficiary_got: U256,
        /// The balance of the pool, the account at [`ARKIV_ADDRESS`].
        pool_got: U256,
        /// Whether the branch holds an account record for the pool.
        pool_exists: bool,
    }

    const HOST_SENDER: Address = Address::repeat_byte(0xa1);
    const HOST_BENEFICIARY: Address = Address::repeat_byte(0xb0);
    const HOST_FUNDS: u128 = 1_000_000_000_000_000_000;
    const HOST_BASE_FEE: u64 = 7;
    const HOST_GAS_PRICE: u128 = 10;

    #[test]
    fn block_pricing_captures_the_schedule_and_rejects_a_stale_base() {
        use arkiv_interfaces::store::Store;
        use reth_ethereum::evm::revm::database_interface::EmptyDB;

        let store = host_store(1);
        let mut evm = ArkivEvmFactory::new(
            store.clone(),
            Arc::new(BlockSeals::default()),
            OpCost::Placeholder,
        )
        .create_evm(EmptyDB::default(), EvmEnv::default());
        let pricing = evm.pricing.clone().unwrap();
        assert_eq!(pricing.commit_id, store.head());
        assert_eq!(pricing.schedule.capacity_rate, 14);

        let advanced = store.begin(None).unwrap();
        store.commit(advanced).unwrap();
        assert!(evm.transact(TxEnv::default()).is_err());
        assert!(evm.branch.is_none(), "a stale base opens no branch");
        assert_eq!(
            evm.pricing.unwrap().schedule.capacity_rate,
            14,
            "an acquired schedule stays as it was"
        );
    }

    /// A branch opened after the head moved past the prices' commit is refused,
    /// even when the head check before it passed.
    #[test]
    fn a_branch_over_another_base_than_the_prices_is_refused() {
        use arkiv_interfaces::store::Store;
        use reth_ethereum::evm::revm::database_interface::EmptyDB;

        let store = host_store(1);
        let mut evm = ArkivEvmFactory::new(
            store.clone(),
            Arc::new(BlockSeals::default()),
            OpCost::Placeholder,
        )
        .create_evm(EmptyDB::default(), EvmEnv::default());
        let base = evm.pricing.clone().unwrap().commit_id;

        let advanced = store.begin(None).unwrap();
        store.commit(advanced).unwrap();

        let Err(EVMError::Custom(error)) = evm.branch(base) else {
            panic!("a branch over a newer head than the prices must be refused");
        };
        assert_eq!(error, "pricing base is no longer the store head");
        assert!(evm.branch.is_none(), "the refused branch is not kept");
    }

    /// The in-memory mock keeps no price list, so a block on it is priced with
    /// [`FeeSchedule::MOCK`].
    #[test]
    fn a_mock_store_is_priced_with_the_mock_schedule() {
        use arkiv_interfaces::store::{Store, StoreExt};
        let store: HostStore =
            std::sync::Arc::new(arkiv_interfaces::store::reference::MemStore::new());
        let genesis = store.begin(None).unwrap();
        store.commit_tagged(genesis, [0; 32]).unwrap();
        assert_eq!(
            BlockPricing::load(&store, store.head()).unwrap().schedule,
            FeeSchedule::MOCK
        );
    }

    /// An in-memory GolemDB with every engine weight set to `weight`, the
    /// test fee schedule, and a funded sender at genesis.
    fn host_store(weight: u64) -> HostStore {
        use arkiv_reth_statemanager::{GenesisAccount, seed_genesis};

        let mut pricing = arkiv_golemdb::PricingConfig::zero();
        pricing
            .weights
            .values_mut()
            .for_each(|value| *value = weight);
        pricing.host_code_version = arkiv_tokenomics::schedule::HOST_CODE_VERSION;
        pricing.weights.extend(
            arkiv_tokenomics::schedule::HOST_WEIGHT_NAMES
                .into_iter()
                .zip([14, 1, 1, 64, 64, 1_000_000])
                .map(|(name, value)| (name.to_owned(), value)),
        );
        let limit = |bytes: usize| u32::try_from(bytes).unwrap();
        let genesis = arkiv_golemdb::Genesis::new(
            arkiv_golemdb::HashAlgorithm::Keccak256,
            arkiv_golemdb::CellLimits {
                max_cell_name_len: limit(arkiv_interfaces::constants::MAX_ATTRIBUTE_NAME_BYTES),
                max_str_len: limit(arkiv_interfaces::constants::MAX_STR_BYTES),
                max_bytes_len: limit(arkiv_interfaces::constants::MAX_BYTES_VALUE_LEN),
            },
            arkiv_golemdb::RecordLimits {
                max_cells: arkiv_interfaces::entity_records::MAX_RECORD_CELLS as u64,
                max_indexed_cells: arkiv_interfaces::entity_records::MAX_RECORD_INDEXED_CELLS
                    as u64,
            },
            None,
            pricing,
        );
        let store: HostStore = std::sync::Arc::new(
            arkiv_golemdb::GolemDbStore::open_memory(&arkiv_golemdb::GolemDbConfig {
                genesis,
                segments: vec![],
                host_schemas: vec![arkiv_golemdb::HostMeteringSchema {
                    code_version: arkiv_tokenomics::schedule::HOST_CODE_VERSION,
                    weight_names: arkiv_tokenomics::schedule::HOST_WEIGHT_NAMES
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                }],
            })
            .expect("open golemdb"),
        );
        seed_genesis(
            &store,
            [0x11; 32],
            [GenesisAccount {
                address: HOST_SENDER,
                balance: U256::from(HOST_FUNDS),
                nonce: 0,
            }],
        )
        .expect("seed")
        .expect("fresh");
        store
    }

    /// Run `ops` as one entity transaction on a fresh block branch over
    /// `store`, as reth would, and read back what it charged and credited.
    fn run_entity_tx(
        store: &HostStore,
        ops: Vec<arkiv_bindings::Operation>,
        charge: bool,
    ) -> Settled {
        use alloy_sol_types::SolCall;
        run_call(
            store,
            IEntityRegistry::executeCall { ops }.abi_encode(),
            U256::ZERO,
            charge,
        )
    }

    /// Run a call to [`ARKIV_ADDRESS`] with `data` and `value` through the
    /// whole entry, like [`run_entity_tx`].
    fn run_call(store: &HostStore, data: Vec<u8>, value: U256, charge: bool) -> Settled {
        let (branch, out) = transact(store, call_tx(data, value), charge, None);
        let out = out.expect("the transaction executes");
        let view = branch_view(store, branch);
        let balance = |address: Address| {
            U256::from_be_bytes(
                view.get_balance(address.into_array(), ReadMode::ViewWithOverlay)
                    .unwrap()
                    .to_be_bytes(),
            )
        };
        Settled {
            success: out.result.is_success(),
            gas_used: out.result.tx_gas_used(),
            sender_paid: U256::from(HOST_FUNDS) - balance(HOST_SENDER),
            beneficiary_got: balance(HOST_BENEFICIARY),
            pool_got: balance(ARKIV_ADDRESS),
            pool_exists: {
                use arkiv_interfaces::store::{ReadTarget, Store};
                store
                    .get(
                        ReadTarget::Branch(branch),
                        arkiv_golemdb_state::accounts::record_key(ARKIV_ADDRESS.into_array()),
                        None,
                        None,
                    )
                    .unwrap()
                    .value
                    .is_some()
            },
        }
    }

    /// A view of `branch` over `store`, to read back what a transaction left.
    fn branch_view(
        store: &HostStore,
        branch: BranchId,
    ) -> arkiv_golemdb_state::GolemStateView<HostStore> {
        use arkiv_interfaces::store::Store;
        arkiv_golemdb_state::GolemStateView::new(
            store.clone(),
            branch,
            store.head(),
            CLAMP_GENESIS,
            arkiv_interfaces::statemanager::SessionId([9; 16]),
        )
    }

    /// A call to [`ARKIV_ADDRESS`] from the funded sender with ample gas.
    fn call_tx(data: Vec<u8>, value: U256) -> TxEnv {
        TxEnv {
            caller: HOST_SENDER,
            gas_limit: 10_000_000,
            gas_price: HOST_GAS_PRICE,
            kind: TxKind::Call(ARKIV_ADDRESS),
            data: data.into(),
            value,
            chain_id: Some(1),
            ..TxEnv::default()
        }
    }

    /// Every sender check on, as for a mined transaction.
    fn all_checks() -> SenderChecks {
        SenderChecks {
            nonce: true,
            fees: FeeChecks {
                base_fee: true,
                priority_fee: true,
                balance: true,
                charge: true,
            },
        }
    }

    /// Run `tx` through the whole entry on a fresh block branch, with `checks`
    /// as reth would pass them, and return the branch with the entry's result.
    #[allow(clippy::type_complexity)]
    fn transact(
        store: &HostStore,
        tx: TxEnv,
        charge: bool,
        checks: Option<SenderChecks>,
    ) -> (
        BranchId,
        Result<ResultAndState<HaltReason>, EVMError<core::convert::Infallible>>,
    ) {
        use arkiv_interfaces::store::Store;

        let branch = store.begin(Some(store.head())).expect("begin");
        let mut db = reth_ethereum::evm::revm::database_interface::EmptyDBTyped::<
            core::convert::Infallible,
        >::default();
        let fees = FeeEnv {
            base_fee: HOST_BASE_FEE,
            beneficiary: HOST_BENEFICIARY,
            charge,
            solvency_checked: true,
        };
        (
            branch,
            arkiv_transact(
                store,
                branch,
                &BlockPricing::load(store, store.head()).unwrap().schedule,
                OpCost::Placeholder,
                &mut db,
                1,
                fees,
                checks,
                tx,
            ),
        )
    }

    /// Whether `branch` holds exactly what a fresh branch at head holds.
    fn untouched(store: &HostStore, branch: BranchId) -> bool {
        use arkiv_interfaces::store::Store;
        let fresh = store.begin(Some(store.head())).expect("begin");
        branch_view(store, branch).digest().unwrap() == branch_view(store, fresh).digest().unwrap()
    }

    /// GolemDB that prices nothing, and one that charges 1 per unit of work.
    const FREE: u64 = 0;
    const PRICED: u64 = 1;

    fn create() -> arkiv_bindings::Operation {
        arkiv_bindings::Operation::create(0, 0, 100, 0, Vec::new())
    }

    /// Through the whole entry, on the host's view, GolemDB's work reaches the
    /// gas reth records, the sender's debit and the beneficiary's tip.
    ///
    /// On the free store the create pays 102,826: compute, 102,824, and
    /// storage, 2. Storage is the empty record's 1,042 units for its 100 blocks
    /// at 14 micro-gas per unit-block, 1.46 gas, rounded up. Compute is its
    /// `PlaceholderCost` (80,000) and the transaction's terms, `tx_gas` =
    /// 21,000 + 4 × 456 calldata tokens = 22,824, above the floor of 21,000 +
    /// 10 × 456 = 25,560. The 420 calldata bytes hold 12 non-zero ones: 3 of
    /// the selector `0x49650044`, one in each of the six head words (array
    /// offset, length, element offset, op tag, bytes offset, bytes length), and
    /// 3 in the 224-byte `Create` payload (its offset, `minLifetime` 100, the
    /// attributes offset). So 408 + 4 × 12 = 456 tokens.
    ///
    /// On a GolemDB that charges for its work, the transaction costs more by
    /// what GolemDB reports, and settlement still adds up.
    #[test]
    fn golemdbs_work_reaches_settlement() {
        let free = run_entity_tx(&host_store(FREE), vec![create()], true);
        let priced = run_entity_tx(&host_store(PRICED), vec![create()], true);
        assert!(free.success && priced.success);
        assert_eq!(free.gas_used, 102_824 + 2);
        assert!(priced.gas_used > free.gas_used, "GolemDB's work is charged");
        for settled in [&free, &priced] {
            let gas = U256::from(settled.gas_used);
            assert_eq!(settled.sender_paid, gas * U256::from(HOST_GAS_PRICE));
            assert_conserved(settled, 2, U256::ZERO);
        }
    }

    /// Where the sender's money went: the tip to the beneficiary, the base-fee
    /// share of `storage_gas` and `value` to the pool, the rest of the base fee
    /// burned. Nothing else moves, so the three add up to the sender's debit.
    fn assert_conserved(settled: &Settled, storage_gas: u64, value: U256) {
        let gas = U256::from(settled.gas_used);
        let base_fee = U256::from(HOST_BASE_FEE);
        let tip = U256::from(HOST_GAS_PRICE - u128::from(HOST_BASE_FEE));
        let burned = base_fee * (gas - U256::from(storage_gas));
        assert_eq!(settled.beneficiary_got, tip * gas, "the tip");
        assert_eq!(
            settled.pool_got,
            base_fee * U256::from(storage_gas) + value,
            "the pool's credit"
        );
        assert_eq!(
            settled.sender_paid,
            burned + settled.beneficiary_got + settled.pool_got,
            "sender debit = burn + tip + pool credit"
        );
    }

    /// The value a successful entity transaction is sent goes to the pool,
    /// beside the storage share: a create's 2 storage gas at base fee 7, 14.
    #[test]
    fn the_pool_gets_the_storage_share_and_the_value() {
        let value = U256::from(1_000);
        let settled = run_call(
            &host_store(FREE),
            IEntityRegistry::executeCall {
                ops: vec![create()],
            }
            .abi_encode(),
            value,
            true,
        );
        assert!(settled.success);
        assert_eq!(settled.pool_got, U256::from(14 + 1_000));
        assert!(settled.pool_exists);
        assert_eq!(
            settled.sender_paid,
            U256::from(settled.gas_used) * U256::from(HOST_GAS_PRICE) + value
        );
        assert_conserved(&settled, 2, value);
    }

    /// A reverted batch keeps its value with the sender, as a revert undoes a
    /// value transfer in the EVM, and buys no storage: the pool gets nothing
    /// and does not come into existence. Calldata that does not decode and a
    /// the same way.
    #[test]
    fn a_reverted_transaction_keeps_its_value_and_credits_no_pool() {
        let ghost = alloy_primitives::B256::repeat_byte(0x42);
        let cases = [
            (
                "a business-rule revert",
                FREE,
                IEntityRegistry::executeCall {
                    ops: vec![arkiv_bindings::Operation::extend_expiry(ghost, 0, 100)],
                }
                .abi_encode(),
            ),
            (
                "a decode failure",
                FREE,
                IEntityRegistry::executeCall::SELECTOR.to_vec(),
            ),
        ];
        for (case, weight, data) in cases {
            let settled = run_call(&host_store(weight), data, U256::from(1_000), true);
            assert!(!settled.success, "{case}");
            assert!(!settled.pool_exists, "{case}: no pool account");
            assert_eq!(
                settled.sender_paid,
                U256::from(settled.gas_used) * U256::from(HOST_GAS_PRICE),
                "{case}: the sender pays gas only"
            );
            assert_conserved(&settled, 0, U256::ZERO);
        }
    }

    /// A view call mined as a transaction is checked like any other before it
    /// writes: a sender who covers the value but not the gas is refused with
    /// the typed invalid-transaction error, and the block's branch is left
    /// exactly as it was, with no debit, no nonce bump, no credit.
    #[test]
    fn a_view_call_the_sender_cannot_pay_for_writes_nothing() {
        let store = host_store(FREE);
        let (branch, out) = transact(
            &store,
            call_tx(
                IEntityRegistry::entityNonceCall { owner: HOST_SENDER }.abi_encode(),
                U256::from(HOST_FUNDS - 1),
            ),
            true,
            Some(all_checks()),
        );
        assert!(
            matches!(
                out,
                Err(EVMError::Transaction(
                    InvalidTransaction::LackOfFundForMaxFee { .. }
                ))
            ),
            "{out:?}"
        );
        assert!(untouched(&store, branch), "the branch is untouched");
    }

    /// A gas limit below the flat charge of a view call or a plain transfer
    /// makes the transaction invalid, before any write, as in Ethereum. A halt
    /// would go into the block with no nonce bump, so it could be included
    /// again.
    #[test]
    fn a_gas_limit_below_the_charge_is_invalid_and_writes_nothing() {
        let view_call = TxEnv {
            gas_limit: 20_000,
            ..call_tx(
                IEntityRegistry::entityNonceCall { owner: HOST_SENDER }.abi_encode(),
                U256::from(1_000),
            )
        };
        let transfer = TxEnv {
            gas_limit: 20_000,
            kind: TxKind::Call(Address::repeat_byte(0xc0)),
            ..call_tx(Vec::new(), U256::from(1_000))
        };
        for (case, tx) in [("a view call", view_call), ("a transfer", transfer)] {
            let store = host_store(FREE);
            let (branch, out) = transact(&store, tx, true, Some(all_checks()));
            assert!(
                matches!(
                    out,
                    Err(EVMError::Transaction(
                        InvalidTransaction::CallGasCostMoreThanGasLimit {
                            gas_limit: 20_000,
                            ..
                        }
                    ))
                ),
                "{case}: {out:?}"
            );
            assert!(untouched(&store, branch), "{case}: the branch is untouched");
        }
    }

    /// A probe moves no value, like its nonce: the pool and the sender stay as
    /// they were.
    #[test]
    fn a_probe_moves_no_value() {
        let settled = run_call(
            &host_store(FREE),
            IEntityRegistry::executeCall {
                ops: vec![create()],
            }
            .abi_encode(),
            U256::from(1_000),
            false,
        );
        assert!(settled.success);
        assert_eq!(settled.sender_paid, U256::ZERO);
        assert_eq!(settled.pool_got, U256::ZERO);
    }

    /// A view call mined as a transaction follows the same rule: its value
    /// goes to the pool when it answers, and stays with the sender when it
    /// reverts. It buys no storage.
    #[test]
    fn a_view_call_sends_its_value_to_the_pool_when_it_answers() {
        let value = U256::from(500);
        let answered = run_call(
            &host_store(FREE),
            IEntityRegistry::entityNonceCall { owner: HOST_SENDER }.abi_encode(),
            value,
            true,
        );
        assert!(answered.success);
        assert_conserved(&answered, 0, value);

        let malformed = run_call(
            &host_store(FREE),
            IEntityRegistry::entityNonceCall::SELECTOR.to_vec(),
            value,
            true,
        );
        assert!(!malformed.success);
        assert_conserved(&malformed, 0, U256::ZERO);
    }

    /// Calldata that does not decode runs no op and pays its terms plus the
    /// start-nonce read, but at least the EIP-7623 floor. With the read free,
    /// the floor (10 per token) is above the terms (4 per token), so it is what
    /// is paid.
    #[test]
    fn undecodable_calldata_pays_its_terms_and_read_but_at_least_the_floor() {
        let data = IEntityRegistry::executeCall::SELECTOR.to_vec();
        assert!(floor_gas(&data) > tx_gas(&data));

        let free = run_call(&host_store(FREE), data.clone(), U256::ZERO, true);
        assert!(!free.success);
        assert_eq!(free.gas_used, floor_gas(&data), "the floor");
    }

    /// A probe reports the same gas as the transaction it estimates, and
    /// settles nothing: no debit, no tip, no pool credit.
    #[test]
    fn a_probe_reports_gas_and_settles_nothing() {
        let charged = run_entity_tx(&host_store(PRICED), vec![create()], true);
        let probe = run_entity_tx(&host_store(PRICED), vec![create()], false);
        assert!(probe.success);
        assert_eq!(probe.gas_used, charged.gas_used);
        assert_eq!(probe.sender_paid, U256::ZERO);
        assert_eq!(probe.beneficiary_got, U256::ZERO);
        assert_eq!(probe.pool_got, U256::ZERO);
    }

    /// `eth_call` and `eth_estimateGas` run on the block's branch, one probe
    /// after another. A probe's writes must not outlive it: a create and a
    /// delete of an entity at genesis both succeed and leave the branch exactly
    /// as they found it, so the next probe sees what the first one did.
    #[test]
    fn a_probe_leaves_the_branch_untouched() {
        use alloy_sol_types::SolCall;
        use arkiv_bindings::Operation;

        let store = host_store(FREE);
        let created = arkiv_interfaces::statemanager::EntityUpdates::create(
            arkiv_interfaces::entity::Entity {
                key: [0x42; 32],
                owner: HOST_SENDER.into_array(),
                expires_at: 1_000,
                ..Default::default()
            },
        );
        {
            use arkiv_interfaces::statemanager::{EntityStore, StateView};
            use arkiv_interfaces::store::Store;
            let branch = store.begin(Some(store.head())).expect("begin");
            let mut view = branch_view(&store, branch);
            view.update_entity(created).expect("stage the entity");
            StateView::commit(&mut view).expect("commit the view");
            store.commit(branch).expect("commit the branch");
        }

        for ops in [
            vec![create()],
            vec![Operation::delete(B256::repeat_byte(0x42))],
        ] {
            let (branch, out) = transact(
                &store,
                call_tx(
                    IEntityRegistry::executeCall { ops }.abi_encode(),
                    U256::ZERO,
                ),
                false,
                None,
            );
            assert!(out.expect("the probe runs").result.is_success());
            assert!(untouched(&store, branch), "the probe left no trace");
        }
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

    /// A transaction the executor refuses is one the payload builder skips, so
    /// the seal must be keyed by the transactions that executed. Keyed by every
    /// transaction tried, the resolver -- which asks by the block's
    /// transactions -- finds no seal, and the node rejects its own block.
    #[test]
    fn seal_keyed_by_transactions_executed() {
        use arkiv_interfaces::store::reference::MemStore;
        use reth_ethereum::evm::revm::database_interface::EmptyDB;

        let store: HostStore = Arc::new(MemStore::new());
        store
            .commit_tagged(store.begin(None).expect("begin"), [0; 32])
            .expect("tag genesis");
        let seals = Arc::new(BlockSeals::default());
        let block_env = BlockEnv {
            number: U256::from(1),
            basefee: 0,
            ..Default::default()
        };
        let mut evm = ArkivEvmFactory::new(store, seals.clone(), OpCost::Placeholder).create_evm(
            EmptyDB::default(),
            EvmEnv {
                cfg_env: CfgEnv::default(),
                block_env,
            },
        );

        let refused = Address::repeat_byte(0xaa);
        let sender = Address::repeat_byte(0xbb);
        let deploy = TxEnv {
            caller: refused,
            kind: TxKind::Create,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        assert!(matches!(
            evm.transact(deploy),
            Err(EVMError::Transaction(_))
        ));
        let transfer = TxEnv {
            caller: sender,
            kind: TxKind::Call(Address::repeat_byte(0xde)),
            gas_limit: ARKIV_TX_GAS,
            ..Default::default()
        };
        evm.transact(transfer).expect("transfer");
        evm.finish();

        let included = ExecutionKey::new(1, vec![(sender.into_array(), 0)]);
        assert!(
            seals.root_of(&included).is_some(),
            "the seal is keyed by the block's transactions"
        );
    }

    /// An RPC call, payload prewarming or an aborted build drops its EVM
    /// without `finish`. The branch has to go with it, or the store keeps it.
    #[test]
    fn an_evm_dropped_without_finishing_discards_its_branch() {
        use arkiv_interfaces::store::{Store, StoreError, StoreExt, reference::MemStore};
        use reth_ethereum::evm::revm::database_interface::EmptyDB;

        let store: HostStore = Arc::new(MemStore::new());
        store
            .commit_tagged(store.begin(None).expect("begin"), [0; 32])
            .expect("tag genesis");
        let mut evm = ArkivEvmFactory::new(
            store.clone(),
            Arc::new(BlockSeals::default()),
            OpCost::Placeholder,
        )
        .create_evm(
            EmptyDB::default(),
            EvmEnv {
                cfg_env: CfgEnv::default(),
                block_env: BlockEnv {
                    number: U256::from(1),
                    basefee: 0,
                    ..Default::default()
                },
            },
        );
        evm.transact(TxEnv {
            caller: Address::repeat_byte(0xbb),
            kind: TxKind::Call(Address::repeat_byte(0xde)),
            gas_limit: ARKIV_TX_GAS,
            ..Default::default()
        })
        .expect("transfer");
        let branch = evm.branch.as_ref().expect("opened by the transfer").id();

        drop(evm);

        assert_eq!(store.discard(branch), Err(StoreError::HandleInvalid));
    }
}
