//! Arkiv execution seam for reth — **no-EVM** executor.
//!
//! This crate owns the one component arkiv-node overrides on top of reth's host:
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
//! - **Plain transfer** — debit sender (value + flat 21k gas), bump nonce,
//!   credit recipient. Computed in Rust, committed as a `BundleState`.
//! - **Call to [`ARKIV_ADDRESS`]** — the entity-engine routing point. Today a
//!   stub (same value/gas accounting, no entity logic yet); the
//!   `arkiv-entitydb` `StateAdapter` STF lands here next.
//! - **Contract creation** — rejected (neutered): user programs never execute.
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
//! [`arkiv_interfaces::execution::TransactionExecutor`] interface. When the entity
//! engine replaces the transfer stub, [`arkiv_transact`] drives
//! [`arkiv::ArkivExecutor`] over an [`arkiv_interfaces::state::EntityStore`] view
//! of reth's database.

/// Entity business logic, implementing the `arkiv-interfaces` executor interface.
pub mod arkiv;

pub use arkiv::ArkivExecutor;

use alloy_evm::{Evm, EvmFactory, eth::EthEvmContext, precompiles::PrecompilesMap};
use alloy_primitives::{Address, Bytes, TxKind, U256, address};
use reth_ethereum::{
    EthPrimitives,
    chainspec::ChainSpec,
    evm::{
        EthEvm, EthEvmConfig,
        primitives::{Database, EvmEnv},
        revm::{
            MainBuilder, MainContext,
            context::{BlockEnv, CfgEnv, Context, TxEnv},
            context_interface::result::{
                EVMError, ExecutionResult, HaltReason, Output, ResultAndState, ResultGas,
                SuccessReason,
            },
            inspector::{Inspector, NoOpInspector},
            interpreter::interpreter::EthInterpreter,
            precompile::Precompiles,
            primitives::hardfork::SpecId,
            state::{Account, EvmState},
        },
    },
    node::{
        api::{FullNodeTypes, NodeTypes},
        builder::{BuilderContext, components::ExecutorBuilder},
    },
};

/// The Arkiv address — `0x4400…0044`.
///
/// Matches the SDK's `ARKIV_ADDRESS`: EOAs `CALL` here with entity
/// `execute(Operation[])` calldata. There is no precompile object and no
/// bytecode — calls to this address are routed directly in [`arkiv_transact`].
pub const ARKIV_ADDRESS: Address = address!("0x4400000000000000000000000000000000000044");

/// Flat gas charged per transaction by the fixed-function executor.
const ARKIV_TX_GAS: u64 = 21_000;

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
    fn transact_raw(
        &mut self,
        tx: TxEnv,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
        arkiv_transact(self.inner.db_mut(), tx)
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

/// The fixed-function state-transition function — Rust, no EVM.
///
/// Reads accounts through the `Database`, applies the transfer / entity-call
/// accounting, and returns a `ResultAndState` (execution result + the account
/// diff). The caller (`EthBlockExecutor::commit_transaction`) commits the diff
/// into the `State`, producing the `BundleState` reth hashes into the state root.
fn arkiv_transact<DB: Database>(
    db: &mut DB,
    tx: TxEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    // Neuter: user programs never execute.
    let to = match tx.kind {
        TxKind::Call(addr) => addr,
        TxKind::Create => {
            return Err(EVMError::Custom(
                "contract creation is disabled (no-EVM Arkiv executor)".to_string(),
            ));
        }
    };

    // A call to ARKIV_ADDRESS is the entity-engine routing point (stub today).
    if to == ARKIV_ADDRESS {
        tracing::debug!(target: "arkiv::executor", caller = %tx.caller, "entity call routed (no EVM)");
    }

    let gas_cost = U256::from(ARKIV_TX_GAS).saturating_mul(U256::from(tx.gas_price));
    let value_out = if to == tx.caller {
        U256::ZERO
    } else {
        tx.value
    };

    // Sender: debit value + gas, bump nonce, mark touched.
    let mut sender = db
        .basic(tx.caller)
        .map_err(EVMError::Database)?
        .unwrap_or_default();
    sender.balance = sender
        .balance
        .saturating_sub(value_out)
        .saturating_sub(gas_cost);
    sender.nonce = sender.nonce.saturating_add(1);
    let mut sender_acc = Account::from(sender);
    sender_acc.mark_touch();

    let mut state = EvmState::default();
    state.insert(tx.caller, sender_acc);

    // Recipient: credit value.
    if to != tx.caller {
        let mut recipient = db
            .basic(to)
            .map_err(EVMError::Database)?
            .unwrap_or_default();
        recipient.balance = recipient.balance.saturating_add(tx.value);
        let mut recipient_acc = Account::from(recipient);
        recipient_acc.mark_touch();
        state.insert(to, recipient_acc);
    }

    let result = ExecutionResult::Success {
        reason: SuccessReason::Stop,
        gas: ResultGas::default().with_total_gas_spent(ARKIV_TX_GAS),
        logs: Vec::new(),
        output: Output::Call(Bytes::new()),
    };

    Ok(ResultAndState::new(result, state))
}

// ---------------------------------------------------------------------------
// Factory + executor builder (the reth injection points)
// ---------------------------------------------------------------------------

/// Custom EVM factory for Arkiv: builds [`ArkivEvm`] (the no-EVM engine) on top
/// of reth's stock EVM context.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ArkivEvmFactory;

impl EvmFactory for ArkivEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> = ArkivEvm<DB, I>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
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
        }
    }
}

/// Builds the Arkiv block executor: reth's stock Ethereum block executor driven
/// by [`ArkivEvmFactory`]. This is the type arkiv-node hands to
/// `EthereumNode::components().executor(..)`.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct ArkivExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for ArkivExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = EthEvmConfig<ChainSpec, ArkivEvmFactory>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        tracing::info!(
            target: "arkiv::executor",
            arkiv_address = %ARKIV_ADDRESS,
            "Assembling Arkiv no-EVM executor over reth host (interpreter bypassed)",
        );
        Ok(EthEvmConfig::new_with_evm_factory(
            ctx.chain_spec(),
            ArkivEvmFactory::default(),
        ))
    }
}
