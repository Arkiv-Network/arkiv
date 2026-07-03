//! `arkiv-executor`: reth executor seam — no-EVM, entity-engine backed.
//!
//! Overrides reth's executor component via
//! `EthereumNode::components().executor(ArkivExecutorBuilder)`. All
//! user transactions are handled by [`arkiv_transact`]:
//!
//! - **Plain transfer** — sender debit + nonce bump, recipient credit.
//! - **Call to [`ARKIV_ADDRESS`]** — raw calldata forwarded to
//!   [`arkiv_db_engine::dispatch`]; the executor never touches ABI types.
//! - **Contract creation** — rejected (neutered).
//!
//! System-contract calls (EIP-4788 / EIP-2935) are delegated to revm.

mod state_adapter;

use alloy_evm::{eth::EthEvmContext, precompiles::PrecompilesMap, Evm, EvmFactory};
use alloy_primitives::{Address, Bytes, Log, TxKind, U256};
use arkiv_db_engine::{ARKIV_ADDRESS, CallResult};
use reth_ethereum::{
    chainspec::ChainSpec,
    evm::{
        primitives::{Database, EvmEnv},
        revm::{
            context::{BlockEnv, CfgEnv, Context, Journal, TxEnv},
            context_interface::{
                JournalTr,
                journaled_state::account::JournaledAccountTr,
                result::{
                    EVMError, ExecutionResult, HaltReason, OutOfGasError, Output, ResultAndState,
                    ResultGas, SuccessReason,
                },
            },
            inspector::{Inspector, NoOpInspector},
            interpreter::interpreter::EthInterpreter,
            precompile::Precompiles,
            primitives::hardfork::SpecId,
            state::EvmState,
            MainBuilder, MainContext,
        },
        EthEvm, EthEvmConfig,
    },
    node::{
        api::{FullNodeTypes, NodeTypes},
        builder::{components::ExecutorBuilder, BuilderContext},
    },
    EthPrimitives,
};

use state_adapter::JournalStateAdapter;

/// Computes the minimum gas that passes reth's tx-pool IntrinsicGasTooLow check.
///
/// The pool enforces two conditions (validate/eth.rs):
///   gas_limit >= initial_total_gas  (21_000 + 4*zero + 16*nonzero)
///   gas_limit >= floor_gas          (EIP-7623 Prague: 10 * (zero + 4*nonzero))
/// We return max(initial, floor) so eth_estimateGas always satisfies both.
fn intrinsic_gas(data: &Bytes) -> u64 {
    let zero: u64 = data.iter().filter(|b| **b == 0).count() as u64;
    let nonzero: u64 = data.len() as u64 - zero;
    let tokens = zero + 4 * nonzero;

    // GasParams::initial_tx_gas in revm-context-interface-17.0.1:
    //   initial = tokens * 4 (tx_token_cost) + 21000 (tx_base_stipend)
    //   floor   = tokens * 10 (TOTAL_COST_FLOOR_PER_TOKEN) + 21000 (tx_floor_cost_base_gas)
    // floor > initial whenever tokens > 0, so floor is almost always the binding limit.
    let initial = 21_000 + 4 * tokens;
    let floor = 21_000 + 10 * tokens;

    initial.max(floor)
}

// ---------------------------------------------------------------------------
// The no-EVM execution engine
// ---------------------------------------------------------------------------

/// Arkiv's EVM replacement.
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

    fn transact_raw(
        &mut self,
        tx: TxEnv,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
        let block_number: u64 = self.inner.block().number.saturating_to();
        let chain_id = self.inner.chain_id();
        let journal = &mut self.inner.ctx_mut().journaled_state;
        arkiv_transact(journal, tx, block_number, chain_id)
    }

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

// ---------------------------------------------------------------------------
// Fixed-function state-transition function
// ---------------------------------------------------------------------------

fn arkiv_transact<DB: Database>(
    journal: &mut Journal<DB>,
    tx: TxEnv,
    block_number: u64,
    chain_id: u64,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    // Neuter: user programs never execute.
    let to = match tx.kind {
        TxKind::Call(addr) => addr,
        TxKind::Create => {
            return Err(EVMError::Custom(
                "contract creation is disabled (no-EVM Arkiv executor)".to_string(),
            ))
        }
    };

    // Sender accounting: debit value + intrinsic gas, bump nonce.
    let gas_used = intrinsic_gas(&tx.data);

    tracing::debug!(
        target: "arkiv::executor",
        tx_gas_limit = tx.gas_limit,
        intrinsic = gas_used,
        data_len = tx.data.len(),
        "arkiv_transact"
    );

    // During eth_estimateGas the binary search probes gas limits below intrinsic.
    // Return Halt so the search raises its lower bound; otherwise it converges to
    // a value < intrinsic_gas and the tx-pool rejects with IntrinsicGasTooLow.
    // Nothing is written to the journal, so it stays clean for the next probe.
    if tx.gas_limit < gas_used {
        return Ok(ResultAndState::new(
            ExecutionResult::Halt {
                reason: HaltReason::OutOfGas(OutOfGasError::Basic),
                gas: ResultGas::default(),
                logs: Vec::<Log>::new(),
            },
            EvmState::default(),
        ));
    }

    // `finalize` resets the journal (including spec) after every tx, so set the
    // spec each call to get correct EIP-161 empty-account pruning. This executor
    // is Prague-only (see `create_evm`'s `Precompiles::prague`).
    journal.set_spec_id(SpecId::PRAGUE);

    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(tx.gas_price));
    let value_out = if to == tx.caller { U256::ZERO } else { tx.value };

    // Sender accounting through the journal, applied *before* the checkpoint so
    // it survives an entity-engine revert (gas is burned even on revert).
    {
        let mut sender = journal.load_account_mut(tx.caller).map_err(EVMError::Database)?.data;
        let new_balance = sender.balance().saturating_sub(value_out).saturating_sub(gas_cost);
        sender.set_balance(new_balance);
        sender.bump_nonce();
        sender.touch();
    }

    // Route calls to ARKIV_ADDRESS through the entity-engine dispatcher.
    if to == ARKIV_ADDRESS {
        let ctx = arkiv_db_engine::CallContext {
            caller: tx.caller,
            chain_id,
            block_number,
        };

        // Checkpoint after sender accounting: on revert we roll back the entity
        // writes but keep the burned gas / bumped nonce.
        let checkpoint = journal.checkpoint();
        let dispatch_result = {
            let mut adapter = JournalStateAdapter::new(&mut *journal);
            arkiv_db_engine::dispatch(&mut adapter, &ctx, &tx.data)
        };

        let call_result = match dispatch_result {
            Ok(r) => r,
            Err(e) => {
                // Roll back and finalize so the journal is clean for the next tx.
                journal.checkpoint_revert(checkpoint);
                let _ = journal.finalize();
                return Err(EVMError::Custom(e.to_string()));
            }
        };

        return match call_result {
            CallResult::Success { output, logs } => {
                journal.checkpoint_commit();
                let state = journal.finalize();
                Ok(ResultAndState::new(
                    ExecutionResult::Success {
                        reason: SuccessReason::Return,
                        gas: ResultGas::default().with_total_gas_spent(gas_used),
                        logs,
                        output: Output::Call(output),
                    },
                    state,
                ))
            }
            CallResult::Revert { data } => {
                // Discard entity writes; sender accounting (pre-checkpoint) stays.
                journal.checkpoint_revert(checkpoint);
                let state = journal.finalize();
                Ok(ResultAndState::new(
                    ExecutionResult::Revert {
                        gas: ResultGas::default().with_total_gas_spent(gas_used),
                        logs: Vec::new(),
                        output: data,
                    },
                    state,
                ))
            }
        };
    }

    // Plain transfer: credit recipient (sender already debited above).
    if to != tx.caller {
        journal.balance_incr(to, tx.value).map_err(EVMError::Database)?;
    }

    let state = journal.finalize();
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

// ---------------------------------------------------------------------------
// Factory + executor builder (the reth injection points)
// ---------------------------------------------------------------------------

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
        let inner = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(Precompiles::prague()));

        ArkivEvm { inner: EthEvm::new(inner, false) }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let inner = self.create_evm(db, input).inner.into_inner().with_inspector(inspector);
        ArkivEvm { inner: EthEvm::new(inner, true) }
    }
}

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
        Ok(EthEvmConfig::new_with_evm_factory(ctx.chain_spec(), ArkivEvmFactory::default()))
    }
}
