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
//! - **Plain transfer** — debit sender (value + flat 21k gas), bump nonce,
//!   credit recipient. Computed in Rust, committed as a `BundleState`.
//! - **Call to [`ARKIV_ADDRESS`]** — the entity engine. The `execute(Operation[])`
//!   calldata is decoded to entity [`Op`]s and applied by [`ArkivExecutor`] over a
//!   reth-backed entity store, returning the account diff reth commits (plus the
//!   sender's gas charge) — see [`arkiv_transact`]. The one view function,
//!   `nonces(address)`, is answered directly from the minting-nonce slot — see
//!   [`arkiv_nonces_call`].
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
//! [`arkiv_interfaces::execution::TransactionExecutor`] interface. [`arkiv_transact`]
//! drives [`arkiv::ArkivExecutor`] over a reth-backed
//! [`arkiv_interfaces::state::EntityStore`] (`RethEntityStore` → `CodeBackend` →
//! [`ExecutorState`]) and returns the diff for reth to commit.

/// Entity business logic, implementing the `arkiv-interfaces` executor interface.
pub mod arkiv;
/// ABI op decoding: `execute(Operation[])` calldata → the spec's `Op`s.
pub mod decode;
/// Revert-payload encoding: `RevertReason` / `DecodeError` → Solidity error data.
pub mod revert;
/// The reth write-path bridge: `AccountCode` over the `Database` + `EvmState` diff.
pub mod state;

pub use arkiv::{ArkivExecutor, OpEffect};
pub use decode::{DecodeError, decode_ops, derive_entity_key};
pub use state::ExecutorState;

use alloy_evm::{Evm, EvmFactory, eth::EthEvmContext, precompiles::PrecompilesMap};
use alloy_primitives::{Address, B256, Bytes, Log, TxKind, U256, address};
use alloy_sol_types::{SolCall, SolError, SolEvent};
use arkiv_bindings::IEntityRegistry;
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
                EVMError, ExecutionResult, HaltReason, OutOfGasError, Output, ResultAndState,
                ResultGas, SuccessReason,
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

use arkiv_interfaces::execution::{BlockDraft, ExecEnv, ExecStatus, Op, OpKind};
use arkiv_interfaces::state::{AuxiliaryStore, EntityStore};
use arkiv_reth_auxstore::RethAuxStore;
use arkiv_reth_entitystore::{CodeBackend, RethEntityStore};

/// The Arkiv address — `0x4400…0044`.
///
/// Matches the SDK's `ARKIV_ADDRESS`: EOAs `CALL` here with entity
/// `execute(Operation[])` calldata. There is no precompile object and no
/// bytecode — calls to this address are routed directly in [`arkiv_transact`].
pub const ARKIV_ADDRESS: Address = address!("0x4400000000000000000000000000000000000044");

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
        let block_number = self.inner.block().number.saturating_to::<u64>();
        arkiv_transact(self.inner.db_mut(), block_number, tx)
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
    block_number: u64,
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

    // A call to ARKIV_ADDRESS is either the `nonces(address)` view or the
    // entity state transition (`execute(Operation[])` — the only other
    // selector `decode_ops` accepts).
    if to == ARKIV_ADDRESS {
        if tx.data.starts_with(&IEntityRegistry::noncesCall::SELECTOR) {
            return arkiv_nonces_call(db, &tx);
        }
        return arkiv_entity_transact(db, block_number, &tx);
    }

    // Otherwise it's a plain value transfer. 21k flat, floored at the calldata
    // intrinsics should the transfer carry data.
    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(tx.gas_price));
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
        gas: ResultGas::default().with_total_gas_spent(gas_used),
        logs: Vec::new(),
        output: Output::Call(Bytes::new()),
    };

    Ok(ResultAndState::new(result, state))
}

/// The entity state transition for a call to [`ARKIV_ADDRESS`].
///
/// Reads the caller's minting nonce, decodes the `Operation[]` calldata, runs the
/// batch over a diff-backed store, and returns the `EvmState` reth commits — plus
/// the sender's gas charge and nonce bump. A business-rule revert charges gas but
/// stages no entity changes; a decode fault reverts likewise.
fn arkiv_entity_transact<DB: Database>(
    db: &mut DB,
    block_number: u64,
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

    // Entity phase — borrows `db` until `into_state` releases it.
    let mut state = ExecutorState::new(db);
    let start_nonce = state
        .read_nonce(caller)
        .map_err(|e| EVMError::Custom(format!("read nonce: {e}")))?;
    let outcome = match decode_ops(&env, &tx.data, start_nonce) {
        Ok(ops) => run_ops(state, &env, ops).map_err(|e| EVMError::Custom(e.to_string()))?,
        Err(e) => Outcome {
            gas_used: 0,
            revert: Some(revert::decode_revert_data(&e)),
            entity_state: state.into_state(),
            logs: Vec::new(),
        },
    };

    // Sender phase — `db` is free again: charge gas, bump the EOA nonce. The
    // charged/reported figure is floored at the pool's intrinsic minimum so a
    // cheap batch still estimates to a pool-acceptable gas limit.
    let gas_used = outcome.gas_used.max(floor);
    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(tx.gas_price));
    let mut sender = db
        .basic(caller)
        .map_err(EVMError::Database)?
        .unwrap_or_default();
    sender.balance = sender.balance.saturating_sub(gas_cost);
    sender.nonce = sender.nonce.saturating_add(1);
    let mut sender_acc = Account::from(sender);
    sender_acc.mark_touch();

    let mut evm_state = outcome.entity_state;
    evm_state.insert(caller, sender_acc);

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

/// Answer the `nonces(address)` view: the queried owner's entity-key minting
/// nonce, ABI-encoded as a `uint32`.
///
/// SDKs `eth_call` this before sending creates to predict the keys the batch
/// will mint (`derive_entity_key(chain_id, owner, nonce + i)`), so it reads
/// the same system-account slot the execute path mints from. The only state
/// staged is the sender's flat gas charge + EOA nonce bump — meaningless for
/// an `eth_call` (the diff is discarded) but keeps reth's sender invariants
/// intact if the call ever arrives as a mined transaction.
fn arkiv_nonces_call<DB: Database>(
    db: &mut DB,
    tx: &TxEnv,
) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
    let output = match IEntityRegistry::noncesCall::abi_decode_raw(&tx.data[4..]) {
        Ok(call) => {
            let nonce = ExecutorState::new(db)
                .read_nonce(call.owner)
                .map_err(|e| EVMError::Custom(format!("read nonce: {e}")))?;
            Ok(IEntityRegistry::noncesCall::abi_encode_returns(&nonce))
        }
        // Malformed args revert with the standard `Error(string)` payload.
        Err(e) => {
            Err(alloy_sol_types::Revert::from(format!("invalid nonces calldata: {e}")).abi_encode())
        }
    };

    // Sender phase — mirrors the plain-transfer accounting.
    let gas_used = ARKIV_TX_GAS.max(intrinsic_gas(&tx.data));
    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(tx.gas_price));
    let mut sender = db
        .basic(tx.caller)
        .map_err(EVMError::Database)?
        .unwrap_or_default();
    sender.balance = sender.balance.saturating_sub(gas_cost);
    sender.nonce = sender.nonce.saturating_add(1);
    let mut sender_acc = Account::from(sender);
    sender_acc.mark_touch();
    let mut state = EvmState::default();
    state.insert(tx.caller, sender_acc);

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

/// What running an op batch produced: the gas metered, the ABI-encoded revert
/// payload if the batch failed a business rule, the entity `EvmState` diff
/// (empty on revert), and the `EntityOperation` logs to emit (empty on revert).
struct Outcome {
    gas_used: u64,
    revert: Option<Bytes>,
    entity_state: EvmState,
    logs: Vec<Log>,
}

/// Run a decoded batch over the reth-backed stores. On success it commits the draft
/// and advances the minting nonce by the number of entities created; on a revert it
/// stages nothing.
fn run_ops<DB: Database>(
    state: ExecutorState<'_, DB>,
    env: &ExecEnv,
    ops: Vec<Op>,
) -> Result<Outcome, eyre::Report> {
    let create_count = ops
        .iter()
        .filter(|o| matches!(o, Op::Create { .. }))
        .count() as u32;
    let caller = Address::from(env.caller);

    let mut store = RethEntityStore::new(CodeBackend::new(state));
    let mut draft = BlockDraft::default();
    let mut effects = Vec::new();
    let out = ArkivExecutor::new()
        .apply_with_effects(env, &mut store, &mut draft, &ops, &mut effects)
        .map_err(|e| eyre::eyre!("apply: {e:?}"))?;

    match out.status {
        ExecStatus::Ok => {
            store
                .apply_delta(&draft.entities)
                .map_err(|e| eyre::eyre!("entity apply_delta: {e:?}"))?;
            let state = store.into_backend().into_inner();

            // Commit the query-index changes over the same state overlay, so the
            // index accounts land in the one `EvmState` diff alongside the entities.
            let mut index = RethAuxStore::new(state);
            index
                .apply_delta(&draft.auxiliary)
                .map_err(|e| eyre::eyre!("index apply_delta: {e:?}"))?;
            let mut state = index.into_backend();

            state.bump_nonce(caller, create_count)?;
            let logs = effects.iter().map(entity_operation_log).collect();
            Ok(Outcome {
                gas_used: out.gas_used,
                revert: None,
                entity_state: state.into_state(),
                logs,
            })
        }
        ExecStatus::Reverted => Ok(Outcome {
            gas_used: out.gas_used,
            revert: out.revert.as_ref().map(revert::revert_data),
            entity_state: store.into_backend().into_inner().into_state(),
            logs: Vec::new(),
        }),
    }
}

/// Encode an [`OpEffect`] as the ABI `EntityOperation` log a client indexes. The
/// entity-hash field is unused for now (`0x0`).
fn entity_operation_log(effect: &OpEffect) -> Log {
    let event = arkiv_bindings::IEntityRegistry::EntityOperation {
        entityKey: B256::from(effect.key),
        operationType: op_type_byte(effect.kind),
        owner: Address::from(effect.owner),
        expiresAt: effect.expires_at.min(u32::MAX as u64) as u32,
        entityHash: B256::ZERO,
    };
    Log {
        address: ARKIV_ADDRESS,
        data: event.encode_log_data(),
    }
}

fn op_type_byte(kind: OpKind) -> u8 {
    use arkiv_bindings::{OP_CREATE, OP_DELETE, OP_EXPIRE, OP_EXTEND, OP_TRANSFER, OP_UPDATE};
    match kind {
        OpKind::Create => OP_CREATE,
        OpKind::Update => OP_UPDATE,
        OpKind::ExtendExpiry => OP_EXTEND,
        OpKind::Transfer => OP_TRANSFER,
        OpKind::Delete => OP_DELETE,
        OpKind::Expire => OP_EXPIRE,
    }
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
/// by [`ArkivEvmFactory`]. This is the type arkiv-reth hands to
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use alloy_sol_types::SolCall;
    use arkiv_bindings::{IEntityRegistry, Mime128, Operation};
    use arkiv_reth_entitystore::decode;
    use arkiv_reth_entitystore::layout::{SYSTEM_ACCOUNT_ADDRESS, entity_address, nonce_slot};
    use reth_ethereum::evm::revm::database_interface::EmptyDB;

    fn create_calldata(btl: u32, payload: &'static [u8]) -> Bytes {
        let mime = Mime128 {
            data: [alloy_primitives::FixedBytes::ZERO; 4],
        };
        IEntityRegistry::executeCall {
            ops: vec![Operation::create(
                btl,
                Bytes::from_static(payload),
                mime,
                vec![],
            )],
        }
        .abi_encode()
        .into()
    }

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

    /// A create call through `arkiv_transact`: the entity is committed at its minted
    /// key, the sender is charged/bumped, and the minting nonce advances — all in the
    /// returned `EvmState`.
    #[test]
    fn entity_create_call_commits_the_entity() {
        let mut db = EmptyDB::default();
        let alice = Address::repeat_byte(0xAA);
        let rs =
            arkiv_transact(&mut db, 10, arkiv_tx(alice, create_calldata(50, b"hello"))).unwrap();

        assert!(rs.result.is_success());

        // The entity landed at the derived key, decodable, with env-resolved fields.
        let key = derive_entity_key(1, &[0xAA; 20], 0);
        let acc = rs
            .state
            .get(&entity_address(key))
            .expect("entity account in the diff");
        let entity = decode(&acc.info.code.as_ref().unwrap().original_bytes()).unwrap();
        assert_eq!(entity.owner, [0xAA; 20]);
        assert_eq!(entity.expires_at, 60); // block 10 + btl 50
        assert_eq!(entity.payload, b"hello");

        // The minting nonce advanced to 1 in the system account.
        let sys = rs
            .state
            .get(&SYSTEM_ACCOUNT_ADDRESS)
            .expect("system account");
        let slot = U256::from_be_bytes(nonce_slot(alice).0);
        assert_eq!(sys.storage.get(&slot).unwrap().present_value, U256::from(1));

        // The sender is touched with its EOA nonce bumped.
        let sender = rs.state.get(&alice).expect("sender account");
        assert_eq!(sender.info.nonce, 1);

        // One EntityOperation log was emitted for the create, at ARKIV_ADDRESS.
        let logs = rs.result.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, ARKIV_ADDRESS);
        let event =
            arkiv_bindings::IEntityRegistry::EntityOperation::decode_log_data(&logs[0].data)
                .unwrap();
        assert_eq!(event.entityKey, B256::from(key));
        assert_eq!(event.operationType, arkiv_bindings::OP_CREATE);
        assert_eq!(event.owner, alice);
        assert_eq!(event.expiresAt, 60);
    }

    /// A create commits the **index** alongside the entity: the new entity's id (0,
    /// the first ever) lands in both the `$all` bucket and its `$owner` bucket, as
    /// roaring bitmaps stored in those accounts' code — all in the one returned diff.
    #[test]
    fn entity_create_commits_index_accounts() {
        use arkiv_interfaces::entity::{AttributeType, annotations};
        use arkiv_reth_auxstore::{Bitmap, all_entities_bucket, pair_address};

        let mut db = EmptyDB::default();
        let alice = Address::repeat_byte(0xAA);
        let rs =
            arkiv_transact(&mut db, 10, arkiv_tx(alice, create_calldata(50, b"hello"))).unwrap();
        assert!(rs.result.is_success());

        // Reads the bitmap stored as an index account's code and checks it holds id 0.
        let bitmap_at = |addr: Address| -> Bitmap {
            let acc = rs
                .state
                .get(&addr)
                .expect("index bucket account in the diff");
            let code = acc
                .info
                .code
                .as_ref()
                .expect("bucket has code")
                .original_bytes();
            Bitmap::from_bytes(code.as_ref()).expect("valid bitmap bytes")
        };

        // Every live entity is in the $all bucket.
        assert!(bitmap_at(all_entities_bucket()).contains(0));
        // And in its owner's bucket (owner value = the 20-byte caller address).
        assert!(
            bitmap_at(pair_address(
                annotations::OWNER,
                AttributeType::EthereumAddress,
                alice.as_slice()
            ))
            .contains(0)
        );
    }

    fn nonces_calldata(owner: Address) -> Bytes {
        IEntityRegistry::noncesCall { owner }.abi_encode().into()
    }

    /// Decode the `uint32` a successful `nonces(address)` call returned.
    fn nonce_from(rs: &ResultAndState<HaltReason>) -> u32 {
        assert!(rs.result.is_success());
        IEntityRegistry::noncesCall::abi_decode_returns(rs.result.output().unwrap())
            .expect("uint32 return")
    }

    /// `nonces(owner)` on a fresh chain answers 0 — and stages nothing beyond
    /// the sender, regardless of who asks about whom.
    #[test]
    fn nonces_call_returns_zero_for_fresh_owner() {
        let mut db = EmptyDB::default();
        let alice = Address::repeat_byte(0xAA);
        let bob = Address::repeat_byte(0xBB);
        let rs = arkiv_transact(&mut db, 10, arkiv_tx(bob, nonces_calldata(alice))).unwrap();

        assert_eq!(nonce_from(&rs), 0);
        // Only the sender (charged/bumped) is in the diff.
        assert_eq!(rs.state.len(), 1);
        assert_eq!(rs.state.get(&bob).expect("sender").info.nonce, 1);
    }

    /// After a create, `nonces` reports 1 for the creator — and still 0 for
    /// anyone else, proving the *decoded argument* is read, not the caller.
    #[test]
    fn nonces_call_reflects_minted_creates() {
        use reth_ethereum::evm::revm::{DatabaseCommit, db::CacheDB};

        let mut db = CacheDB::new(EmptyDB::default());
        let alice = Address::repeat_byte(0xAA);
        let bob = Address::repeat_byte(0xBB);

        let rs =
            arkiv_transact(&mut db, 10, arkiv_tx(alice, create_calldata(50, b"hello"))).unwrap();
        assert!(rs.result.is_success());
        db.commit(rs.state);

        let rs = arkiv_transact(&mut db, 11, arkiv_tx(bob, nonces_calldata(alice))).unwrap();
        assert_eq!(nonce_from(&rs), 1);
        let rs = arkiv_transact(&mut db, 11, arkiv_tx(alice, nonces_calldata(bob))).unwrap();
        assert_eq!(nonce_from(&rs), 0);
    }

    /// The `nonces` selector with truncated arguments reverts.
    #[test]
    fn nonces_call_with_malformed_args_reverts() {
        let mut db = EmptyDB::default();
        let alice = Address::repeat_byte(0xAA);
        let mut data = IEntityRegistry::noncesCall::SELECTOR.to_vec();
        data.extend_from_slice(&[0x01, 0x02]);
        let rs = arkiv_transact(&mut db, 10, arkiv_tx(alice, data.into())).unwrap();
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
        let alice = Address::repeat_byte(0xAA);
        let rs =
            arkiv_transact(&mut db, 10, arkiv_tx(alice, create_calldata(50, b"hello"))).unwrap();
        db.commit(rs.state);

        // An update batch: cheap in the cost model (40k base) but with calldata
        // whose intrinsic floor exceeds it.
        let key = B256::from(derive_entity_key(1, &[0xAA; 20], 0));
        let update = IEntityRegistry::executeCall {
            ops: vec![Operation::update(
                key,
                Bytes::from(vec![0xAB; 4_000]), // 4k nonzero bytes → floor ≈ 181k
                Mime128 {
                    data: [alloy_primitives::FixedBytes::ZERO; 4],
                },
                vec![],
            )],
        }
        .abi_encode();
        let floor = intrinsic_gas(&update);
        let rs = arkiv_transact(&mut db, 11, arkiv_tx(alice, update.into())).unwrap();

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
        let alice = Address::repeat_byte(0xAA);
        let data = create_calldata(50, b"hello");
        let mut tx = arkiv_tx(alice, data.clone());
        tx.gas_limit = intrinsic_gas(&data) - 1;
        let rs = arkiv_transact(&mut db, 10, tx).unwrap();

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
        let alice = Address::repeat_byte(0xAA);
        let rs = arkiv_transact(
            &mut db,
            10,
            arkiv_tx(alice, Bytes::from_static(&[0xDE, 0xAD])),
        )
        .unwrap();

        assert!(!rs.result.is_success());
        assert_eq!(rs.state.get(&alice).expect("sender").info.nonce, 1);
        // Nothing else was staged (only the sender).
        assert_eq!(rs.state.len(), 1);
    }
}
