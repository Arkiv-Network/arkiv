# Tokenomics iteration 1: implementation plan

Status: draft, 2026-10-23. Spec: `docs/tokenomics-spec.md`. GolemDB context:
`golemdb-metering.md` and `write-metering-implementation-plan.md` in the
GolemDB repo.

## Goal

Senders pay for the bytes they store, times the blocks they keep them, and
the base-fee share of that payment goes into a storage pool instead of being burned.
Iteration 1 is an intermediate gas-only milestone, not full compliance with the
[target spec](tokenomics-spec.md). Payment remains one gas charge, but compute,
storage and purge have separate accounting from the start.

Concretely, iteration 1:

1. reads the fee schedule from store cells (values faked for now),
2. passes GolemDB's metered results (receipts, budgets) through to Arkiv,
3. reads each entity's `#meta` counts,
4. prices storage × lifetime and credits its base-fee share to the pool,
5. keeps the compute/storage/purge split in execution results, for settlement and the pool.

From outside, an entity transaction is a normal transaction with one fee: `gasUsed`,
`eth_estimateGas` and the receipt are standard, and Ethereum's mechanics are unchanged. The
split is internal plumbing, so the move to the target tokenomics only changes how it is
charged and reported (decided 2026-10-27).

## Baseline

`main` at `9e678a2`, after PR #222 (golemdb/query_integration):

- GolemDB is pinned at `d910d14`: its calls return `Metered<T>` and take
  explicit budgets, records carry `#meta`, its genesis type has record limits,
  and `golemdb-metering` (#91) brings `Schedule` with `host.*` weights,
  `WriteMeter` and `Receipt.ledger`.
- The Arkiv adapter passes budgets to GolemDB's record calls. Since the local commit
  `d0f48a4` it also maps every receipt (`metered_result`, `arkiv_receipt` in
  `crates/arkiv-golemdb/src/convert.rs`), but the state layer drops them, and Arkiv's
  `Record` does not carry `#meta`.
- GolemDB still charges cost 0 on every call: its write pricing lands with its
  own iterations 4–6.

## Scope

| In | Out (later iterations) |
| --- | --- |
| (GolemDB at `d910d14`: done by #222) | real GolemDB compute costs (still 0) |
| receipts and budgets passed through Arkiv's store layer | purge reserve (`purge_gas` from the D5 bound) |
| `#meta` counts read for entities | priority fee on compute gas only |
| fee schedule as store cells, seeded from the genesis | compute-only block `gasUsed` and base fee |
| storage units, storage gas for create, extend, growing patch | storage and purge paid through `value` at `min_base_fee`, gas for compute only |
| | nonzero purge-reserve collection and pool-funded purge settlement |
| storage share credited to the pool, the balance of `ARKIV_ADDRESS` | `arkiv_getFeeSchedule` |
| | `$rate` per entity (postponed with pool payouts, decided 2026-10-27; see 6d) |
| `max_lifetime` as an entity-operation admission check | pool payouts to validators, expiry ledger, `R` / `L` |
| | `arkiv_estimateFee`; receipt fields `computeGas`, `storageGas`, `purgeGas`, `storageFee`, `purgeFee` (with the target tokenomics, decided 2026-10-27) |
| | limits: `max_block_storage_units`, `max_live_bytes` |

Compute gas stays `PlaceholderCost`. All fee-schedule values are placeholders.
Purge gas is present in the breakdown but zero: this iteration does not yet collect a
deletion reserve. Its presence does not imply that purge funding has been implemented.

## Architecture

### New crate: `arkiv-tokenomics`

Pure, deterministic pricing, depending only on `arkiv-interfaces`: no reth, no
store. It holds the consensus-critical arithmetic in one reviewable place,
like `entity_records` does for the record layout, and is reusable by the RPC
and CLI later.

- `FeeSchedule`: the `host.arkiv.*` values iteration 1 needs.
- Storage units from `#meta` counts and the schedule.
- Unit-blocks per operation; storage fee in wei; conversion to gas.
- `$rate` on create, extend and patch.
- A shared gas breakdown used by settlement, and later by receipts and estimation. Keep the
   three components until settlement rather than passing only a combined gas count.

I/O (reading the schedule, writing `$rate`, crediting the pool) stays in the
state and executor layers. `PlaceholderCost` (`arkiv-interfaces/src/gas.rs`)
can move into the crate in a later iteration.

### Plug-in points

| Concern | Where |
| --- | --- |
| GolemDB API and receipts | `crates/arkiv-golemdb` (adapter), `crates/arkiv-interfaces/src/store.rs` (`Receipt`, `Record`) |
| Entity writes and their receipts | `crates/arkiv-golemdb-state/src/entities.rs` (`store.create` / `patch` / `delete`, receipts dropped today) |
| Genesis values | `crates/arkiv-reth-chainspec/src/lib.rs` (`ArkivConfig`, today only `golemdb`) |
| Genesis seeding (covered by the genesis root) | `crates/arkiv-reth-statemanager/src/genesis.rs` (`seed_genesis`), `bin/arkiv-reth/src/genesis.rs` |
| Gas per operation, staging | `crates/arkiv-reth-executor/src/arkiv.rs` (`apply_with_effects`) |
| Transaction gas, settlement | `crates/arkiv-reth-executor/src/lib.rs` (`arkiv_entity_transact`, `charge_sender`, `FeeEnv`) |
| Entity cells | `crates/arkiv-interfaces/src/entity.rs` (`annotations`), `crates/arkiv-interfaces/src/entity_records.rs` |

## Pricing rules (iteration 1)

From `docs/tokenomics-spec.md`, restricted to storage, with one simplification:
**all fees are collected through gas.** The spec charges gas for compute only,
at `base_fee + priority_fee`, and takes storage and purge through the
transaction's `value`, priced at `min_base_fee`; that is a later iteration. In
iteration 1 storage gas is part of `gas_used` and charged at the effective gas
price like compute gas. The component gas counts do not depend on the base fee;
their wei charges do. Estimates are exact only for the quoted state, schedule and
execution block context, as in the spec. The limitations are listed below.

```
storage_units = cell_bytes × cell_byte_units + index_bytes × index_byte_units
              + cells × c_cell + indexed_cells × c_idx          (all counts from #meta)

storage_time  = per operation, in batch order:
                create:         units_after × (expires_at − block_number)
                extend:         units × (new_expires_at − old_expires_at)
                growing patch:  (units_after − units_before) × (expires_at − block_number)
                otherwise:      0                                (no refunds)

storage_gas   = ceil(Σ storage_time × capacity_rate / 1_000_000)  (checked u128)
compute_gas   = max(tx_intrinsic + tx_data + Σ PlaceholderCost, floor_gas)
purge_gas     = 0                                                (collection deferred)
gas_used      = compute_gas + storage_gas + purge_gas
storage_fee   = base_fee × storage_gas                            (wei: the pool's share)
purge_fee     = base_fee × purge_gas                              (0 in iteration 1)

effective_gas_price = base_fee + priority_fee
sender_charge      = effective_gas_price × gas_used
compute_burn       = base_fee × compute_gas
proposer_payment   = priority_fee × gas_used
pool_credit        = storage_fee + purge_fee
```

### Split accounting, one payment

On a successful transaction, the sender pays `sender_charge` through gas only. The following
identity must hold:

```
sender_charge = compute_burn + proposer_payment + pool_credit
```

A non-zero `value` pays no fee: it is debited on top and goes to the pool as well, as to a
payable contract (decided 2026-10-27, see 6e), so the sender's whole debit is
`sender_charge + value` and the pool receives `pool_credit + value`.

| Component | Gas | Sender's charge | Destination |
| --- | --- | --- | --- |
| Compute | `compute_gas` | `effective_gas_price × compute_gas` | Base fee burned; priority fee to proposer |
| Storage | `storage_gas` | `effective_gas_price × storage_gas` | Base fee to pool; priority fee to proposer |
| Purge reserve | `purge_gas = 0` | 0 | No reserve collected yet |

Receipts are standard: `gasUsed` is the breakdown's total and `effectiveGasPrice` the one
price; the header sums `gasUsed`. The components stay internal in iteration 1 (decided
2026-10-27): receipt fields `computeGas`, `storageGas`, `purgeGas`, `storageFee` and
`purgeFee`, as the target [Fee API](api-spec.md#7-fee-api-draft) has them, come with the
target tokenomics.

On revert, discard the tentative storage/purge purchases and their pool credits: the
breakdown has storage and purge 0, and compute is the gas charged under the failure rules,
including any out-of-gas charge. Failed transactions must never credit the pool for purchases
that were rolled back.

For example, with compute gas 100, storage gas 20, base fee 10 wei and priority fee 2 wei:
the sender pays 1,440 wei, of which 1,000 is burned, 240 goes to the proposer and 200 to the
pool. Purge contributes zero. The component sender charges are 1,200 and 240 wei; the latter
is not the same as the 200-wei storage pool credit.

`tx_intrinsic`, `tx_data` and `floor_gas` are as in the spec (21,000; EIP-2028;
EIP-7623). The floor applies to the compute part only, as in the spec, although
in iteration 1 all gas still counts against the block gas limit. The terms are
added: today the executor charges `max(Σ PlaceholderCost, tx_intrinsic + tx_data,
floor_gas)`, so the transaction terms are not paid whenever the ops cost more.

**Per operation, measured on the branch** (decided 2026-10-26). A batch is priced as the
same operations in sequence; only `tx_gas` is shared, and `storage_gas` is rounded once over
the batch. Costs then depend on the order of operations, as they do across transactions; the
sender controls that order. Every cost is accumulated while the operation's record calls run
on the transaction's branch, in memory, inside one transaction safe point (a view frame):

- `units_before`: from the `#meta` of the record the operation already reads for its
  ownership check; 0 for a create.
- `units_after = units_before + Δ`, with Δ from the write receipt's `WriteDetails`
  (cells created − deleted, cell bytes written − deleted, index joins − leaves, index bytes
  written − deleted): the same effects GolemDB applies to `#meta`.
- **Every backend must report exact `WriteDetails`.** They are a pricing input, not a
  diagnostic: MemStore and Valkey keep a zero *cost* but compute the effects with GolemDB's
  size rule, including its no-op rule (an unchanged cell contributes nothing). Today both
  return zero details (`store/reference.rs` 238, `arkiv-valkey/src/lib.rs` 172), which would
  price every create at zero units.
- `golemdb_cost`: the receipts of the operation's reads and writes, minting-nonce traffic
  included.

No extra reads are needed, and each operation's charge is visible on its own, which keeps
debugging simple.

**Entity-operation admission: `max_lifetime` is in iteration 1.** For create and extend,
validate the resolved future expiry before writing or calculating storage time:
`expires_at - block_number <= max_lifetime` (use `new_expires_at` for extend). This bounds
the total remaining lifetime, not just the added blocks. Exceeding it causes a typed
lifetime-limit revert of the whole batch, with spent compute charged and no storage or purge
purchase retained. This is an execution admission check, not transaction-pool rejection or
an out-of-gas condition. Permanent expiry (`MAX_EXPIRES_AT`) is explicitly rejected by this
check in iteration 1; it has no exemption, and rejection does not rely on arithmetic overflow.

`capacity_rate` is in micro-gas per unit-block: a unit-block is tiny (the
spec's 1 KiB entity for 3 months is about 6 × 10⁹ unit-blocks), so a whole-gas
rate would be far too coarse.

`$rate` is the wei per block an entity's storage was bought at, updated per operation from
that operation's own unrounded purchase, never from the rounded batch total: the formulas
are in step 4, the liability rule in the execution doc's
[Pool Accounting](tokenomics-execution.md#pool-accounting). A delete removes it.

The fee schedule uses `host.arkiv.*` names, as GolemDB's `Schedule` will carry
once it persists host weights: `host.arkiv.capacity_rate`,
`host.arkiv.cell_byte_units`, `host.arkiv.index_byte_units`,
`host.arkiv.c_cell`, `host.arkiv.c_idx`.

## Known limitations

Collecting storage through gas departs from the spec in ways that are
acceptable with placeholder values and a single sequencer, and that the switch
to `value` payment removes:

| Limitation | Effect | Why acceptable for now |
| --- | --- | --- |
| Storage is paid at the full gas price | the storage fee moves with the base fee | `min_base_fee` pricing comes with `value` payment |
| The priority fee also applies to storage gas | the proposer earns in proportion to storage bought, against the spec's principle 3 | with one sequencer, proposer and pool operator are the same party |
| Storage gas counts against the block gas limit and the base fee | large purchases take block capacity and raise the base fee, against principle 1 | dev chains with low load |
| The EIP-7825 cap (16,777,216 gas) includes storage gas | it limits what one transaction can buy; at the starting `capacity_rate` of 14, a 4 KiB entity for 6 months is about 0.5M storage gas, but a 128 KiB entity for 6 months, about 14.3M storage gas plus a 5.26M calldata floor, does not fit one transaction | storage gas is small for typical entities at the starting values (open decision 7) |
| The pool receives `base_fee × storage_gas` | pool income moves with the base fee | `$rate` records the price actually paid, so later accounting stays exact |

**Switching to `value` payment later changes what clients must send.** Once
storage and purge move to `value`, a transaction without enough `value` gets an
`InsufficientValue` revert, and gas figures shrink to compute only. The switch
needs an activation block, like a fork. In iteration 1 the SDK estimates with
`eth_estimateGas` and sends exactly zero value, like any contract call; with the switch it
moves to `arkiv_estimateFee`'s quote-and-sign workflow, which arrives then, and its
`requiredValue` (the storage and purge fee), and receipts gain the component breakdown,
while `gasUsed` changes to compute only. Previously signed gas-only transactions are not
guaranteed to succeed after that switch; clients must re-estimate and re-sign as needed.

## Steps

Each step leaves the full CI pipeline green (fmt, clippy, nextest, Valkey
tests) and is its own commit.

1. **GolemDB bump to `d910d14`.** Done by PR #222: the pin, the adapter
   migration, budgets on record calls, `record_limits` in `genesis/*.json`.

2. **Receipts and budgets through Arkiv.**
   *Done* (`d0f48a4`, #222): Arkiv's `Receipt` carries GolemDB's cost, `priced_at` and
   `WriteDetails`, with `Receipt::accumulate`; the adapter maps every call's receipt, including
   a `get` that finds nothing; budgets reach GolemDB through the adapter; MemStore and Valkey
   report zero receipts. GolemDB's per-weight ledger is not carried: gas does not need it.
   *Remaining:*
   - **2a0. Exact effects on MemStore and Valkey.** One shared function in
     `arkiv-interfaces`, `WriteDetails::between(before_cells, after_cells)`, applies GolemDB's
     size rule and no-op rule; the reference store and Valkey build their receipts with it,
     cost still 0. The same size rule backs `RecordMeta::from_cells` (step 3).
   - **2a. Return receipts from the state layer.** *Implemented (2026-10-26).*
     `GolemStateView`'s entity get, create, patch and delete return their receipts, and the
     minting-nonce read and increment include their reads and writes as entity cost (decided
     2026-10-26), with one account read and one write per increment. 2c turned these into the
     trait methods themselves. Balance and transaction-nonce traffic is settlement, covered by `tx_intrinsic`, and is
     not included in these receipts. Index queries serve read RPCs and are not collected.
     Chargeable costs saturate at `u64::MAX` (revised 2026-10-27, was checked addition
     returning `CostOverflow`): the executor's checked meter turns a cost that large into out
     of gas, where an error would abort the block. `WriteDetails` counters saturate too. Errors do not roll back writes: the caller owns the frame. An absent delete
     still succeeds with a zero receipt pending failed-call receipt support. Tests:
     `arkiv-golemdb-state/src/metering_tests.rs`.
   - **2b. Frame rules in the state layer.** *Implemented (2026-10-26).* One frame per
     transaction; a second `begin_frame` is `ViewError::FrameAlreadyOpen`, because
     `merge_frame` does not merge the store frame and `HostStateView` does not roll back its
     reth account mirror (`host.rs` 292–301). Account writes stay outside the frame. Receipts
     are return values, never view state, so `discard_frame` rolls back writes but not what
     they cost: GolemDB charges rolled-back work (metering R6). The executor opens no frame
     yet; it uses these rules from step 6 on.
   - **2c. Trait access.** The receipt-returning entity calls and the frame methods are on the
     `arkiv-interfaces` state traits, implemented by `GolemStateView` and delegated by
     `HostStateView` (the only two implementations), so `run_ops`, generic over `StateView`,
     uses them and MemStore tests cover them.
     *Implemented (2026-10-26).* No separate metered variants: `get_entity`, `update_entity`,
     `get_entity_creation_nonce` and `fetch_increment_entity_creation_nonce` return
     `Metered<T>`, as `Store` does; callers that do not charge (read-only calls, the purge,
     today's batch path until step 6) say so with `into_value`. `begin_frame`, `merge_frame`
     and `discard_frame` are on `StateView`. The shared conformance suite asserts exact write
     details and the frame rules on both implementations; costs stay in `metering_tests.rs`.
     Step 3 changes `get_entity`'s value once more, to carry `#meta`.
   - Executor plumbing, formerly 2d, moved to step 6. Today's executor writes each entity's
     net change only after every operation succeeded, reading it up to three times per
     operation, so it has no per-operation receipts to carry; step 6 replaces that design.
   *Done when:* the state-layer tests cover 2a–2c, and a cross-store parity test runs the same
   batches on MemStore, Valkey and GolemDB and gets identical `WriteDetails` (creates, patches
   that add, change, remove and leave cells unchanged, deletes).
   *Done (2026-10-26):* the store suite `run_write_details` holds all three stores to the same
   fixed effects, and the shared `StateView` suite runs over `GolemStateView` on MemStore,
   GolemDB (`arkiv-golemdb-state/tests/golemdb.rs`) and Valkey (`tests/valkey.rs`).
   *Deferred, needed before GolemDB charges non-zero costs:* each call's budget from the
   transaction's remaining gas (entity calls pass `None` today); receipts of failed calls,
   which `Result<Metered<T>, StoreError>` cannot carry except as `OutOfBudget{spent}` (an API
   change to `Metered<Result<T, E>>`, as GolemDB has). Patches send every cell, but GolemDB
   skips unchanged ones ("No-ops do not write … or contribute effects", `crud.rs` 121), so
   write details stay exact; only per-request size could inflate cost.

3. **`#meta` into Arkiv.**
   - GolemDB returns `#meta` with every default `get`, on branches (`record/src/crud.rs`
     238–261) as on commits, and counts user cells only (`record/src/meta.rs`). The adapter
     parses it in `arkiv_record` with `RecordMeta::decode`, mapping corruption to
     `StoreError::Internal`, instead of dropping all `#` cells (`Record::meta()` would turn
     corruption into `None`).
   - **GolemDB's `#meta` is the source of the size** (decided 2026-10-26), not a figure Arkiv
     computes from the entity: it is committed state, and it comes from the same engine as the
     receipt's `WriteDetails`, so `units_before + Δ` never mixes two size rules.
   - `arkiv_interfaces::store` gains `RecordMeta`, GolemDB's four counts one to one:

     ```rust
     #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
     pub struct RecordMeta {
         pub cells: u64,
         pub cell_bytes: u64,
         pub indexed_cells: u64,
         pub index_bytes: u64,
     }
     ```

     `Record` gains `meta: Option<RecordMeta>`, optional because a projected read may leave
     `#meta` out. One `RecordMeta::from_cells` in `arkiv-interfaces` applies GolemDB's size
     rule (cell `(8+name)+(1+value)`, index entry `name+2+value`, attributes indexed) with the
     size helpers of 2a0; MemStore and Valkey, which have no engine to ask, fill `meta` with it.
     Adding the field touches every `Record` construction: the adapter, the reference store,
     Valkey, tests.
   - The entity read returns the size with the entity, in one store call. `EntityStore` gains
     a required `get_entity_with_meta`; `get_entity` becomes a provided method derived from it,
     so its signature and its callers stay as they are (decided 2026-10-26):

     ```rust
     /// An entity as the store holds it, and the size its record takes.
     pub struct StoredEntity { pub entity: Entity, pub meta: RecordMeta }

     fn get_entity_with_meta(&self, address: EntityAddress, read: ReadMode)
         -> Result<Metered<Option<StoredEntity>>, Self::Error>;
     fn get_entity(&self, address: EntityAddress, read: ReadMode)
         -> Result<Metered<Option<Entity>>, Self::Error>;   // provided
     ```

     `meta` is not optional in `StoredEntity`: an entity read fetches the whole record, so a
     missing `#meta` is a store fault, not an unknown size. Only step 6's ownership read uses
     it: `meta` is `units_before`, 0 for a create. `GolemStateView` implements
     `get_entity_with_meta`; `HostStateView` passes it through.
   - `$rate` moved to step 6, where it is first written with a value (decided 2026-10-26).
   *Done when:* for created, patched (including a removed cell, a changed type and a field
   becoming an attribute) and deleted entities, GolemDB's stored `#meta` equals
   `RecordMeta::from_cells(to_cells(entity))`, and the receipt's `WriteDetails` equal the
   change in `#meta`; a deleted entity has no meta. `get_entity_with_meta` returns that meta on
   all three stores (the shared `StateView` suite). On MemStore and Valkey the counts hold by
   construction, so the GolemDB comparison is the test of the size rule.
   *Done (2026-10-26):* the store suite's `a_read_reports_the_records_size` and
   `the_size_moves_by_what_each_write_reports` (branch, committed and query reads, projections),
   the adapter's `meta_decodes_or_the_read_fails`, and the `StateView` suite's
   `an_entity_read_reports_its_records_size`, all run on MemStore, GolemDB and Valkey.

4. **`arkiv-tokenomics` crate.** Pure, checked arithmetic, no I/O, depending only on
   `arkiv-interfaces`:
   - `FeeSchedule`: `capacity_rate`, `cell_byte_units`, `index_byte_units`, `c_cell`, `c_idx`,
     with placeholder defaults 14, 1, 1, 64, 64 until step 5 reads them from cells.
  Also carry `max_lifetime` in blocks as an admission limit, separate from fee arithmetic;
  its initial value must be explicitly configured.
   - `storage_units(meta, schedule)`, `units_delta(details)` from `WriteDetails`, and
     `storage_time` per operation, by the rule in [Pricing rules](#pricing-rules-iteration-1).
   - `GasBreakdown`, plain data, lives in `arkiv-interfaces`, so `ExecOutput` can carry it
     without a dependency cycle. `GasMeter` and its `impl` live in `arkiv-tokenomics`, which
     the executor depends on (Rust allows an inherent `impl` only in the type's own crate).
   - `GasMeter`: the fee formula's inputs, summed over one transaction's batch, kept apart from
     the final rounding:

     ```rust
     pub struct GasMeter {
         pub arkiv_op_cost: Gas,   // Σ arkiv_op_cost(op); PlaceholderCost for now
         pub golemdb_cost: Gas,    // Σ entity receipt cost; one cost unit is one gas
         pub storage_time: u128,   // Σ storage_time, in unit-blocks
         pub purge_bound: Gas,     // Σ purge_bound; 0 in iteration 1
     }
     impl GasMeter {
         pub fn add_op_cost(&mut self, gas: Gas) -> Result<(), Overflow>;
         pub fn add_golemdb(&mut self, receipt: &Receipt) -> Result<(), Overflow>; // per call
         pub fn add_storage_time(&mut self, unit_blocks: u128) -> Result<(), Overflow>;
         pub fn add_purge_bound(&mut self, gas: Gas) -> Result<(), Overflow>;
         /// tx_gas + arkiv_op_cost + golemdb_cost, without the floor: the running check.
         pub fn compute_so_far(&self, tx_gas: Gas) -> Result<Gas, Overflow>;
         /// Applies the floor and the one rounding of storage gas per transaction.
         pub fn finish(&self, tx_gas: Gas, floor_gas: Gas, schedule: &FeeSchedule)
             -> Result<GasBreakdown, Overflow>;
     }
     pub struct GasBreakdown {
         pub compute_gas: Gas,     // max(tx_gas + arkiv_op_cost + golemdb_cost, floor_gas)
         pub storage_gas: Gas,     // ceil(storage_time × capacity_rate / 1_000_000)
         pub purge_gas: Gas,       // purge_bound
     }
     ```

     `GasBreakdown` is the one structure settlement uses, and later receipts and
     `arkiv_estimateFee` will.
     The meter sums exact values so the floor and the rounding apply once per transaction,
     as the spec requires; partial results serve the running out-of-gas check and, once
     `value` payment exists, the running `InsufficientValue` check.
   - `$rate`, per operation in batch order, from the operation's own unrounded purchase:

     ```
     added_fee_µ = fee_base × capacity_rate × storage_time(op)   in wei × 10⁶, exact
     rate_kept   = floor(old_rate × min(units_after, units_before) / units_before)   0 on create
     new_rate    = floor((rate_kept × (old_expires_at − block_number) × 1_000_000 + added_fee_µ)
                         / ((new_expires_at − block_number) × 1_000_000))
     ```

     `fee_base` is the base fee in iteration 1 and `min_base_fee` in the spec. The purchase
     keeps its fractional wei (scaled by 10⁶) until the one final division: dividing
     `fee_base × capacity_rate` by 10⁶ first would understate rates. Checked `u128`: the
     intermediate exceeds `u64` already in the worked example (about 8.5 × 10²⁰) and stays below
     `u128` at any realistic input. One rule covers create (`old_rate` 0), extend, growing and shrinking patch, with the
     liability rule (`delta_L`) of the execution doc. A shrinking patch measures both unit
     counts with the weights in force at that block, while the purchase was measured with the
     weights of its time; after a weight change the kept share is deterministic but not the
     purchase-time proportion, accepted for iteration 1. `$rate` is a fixed-width `u64` cell: a
     create writes it as 0 and the operation then sets its value, so the measured units already
     include it (22 bytes + `c_cell`, 86 units at the default weights) and the value write
     changes no size; its receipt counts as `golemdb_cost`.
   - Checked arithmetic throughout: gas `u64`, storage time `u128`, wei amounts `u128`
     (the storage fee, base fee × storage gas, is `u64` × `u64` and cannot overflow it); an
     overflow fails the transaction. Balances stay `U256`: step 6 widens the crate's wei
     amounts losslessly where it applies them to accounts. `Receipt::accumulate` and today's executor sum saturate; `Receipt::accumulate`
     may stay so for diagnostic details, but the cost entering the meter is added checked.
   *Done when:* unit tests reproduce the worked example (create 84,697 storage gas at a
   `capacity_rate` of 14; patch 37,159; extend 210,652 at 21; at a base fee of 10 gwei,
   `$rate` 217,840,000, then 361,200,000, then 496,650,000), batches that differ only in
   order (grow then shrink, extend then shrink, and the reverse), and `units_delta` against
   recorded `WriteDetails`.
   *Done (2026-10-26):* `crates/arkiv-tokenomics`, with `GasBreakdown` in
   `arkiv-interfaces/src/gas.rs`. `tests/worked_example.rs` reproduces every figure above and
   the order cases: at one price a grow and a shrink cost the same in either order, while an
   extend before a shrink buys twice the storage of the reverse. `units_delta` is checked
   against the effects GolemDB's write-details tests pin.

5. **Fee schedule as cells.**
   `config.arkiv.tokenomics` in the chain spec; seeded at genesis into a
   fee-schedule record with one cell per `host.arkiv.*` name; the executor
   reads it from the block's base state behind a small `FeeScheduleSource`
   trait, so GolemDB's pricing snapshot can replace it later.
   **Interim until GolemDB carries host weights** (decided 2026-10-26). GolemDB's design (D9)
   has `host.*` weights stored, validated and activated in its cost schedule and returned in
   the pricing snapshot each call is priced with; GolemDB never prices with them. At
   `d910d14` only the in-memory `Schedule` exists: persisting it (GolemDB iteration 5), runtime
   changes (iteration 8) and an API that returns the snapshot's weights are still to come.
   Arkiv's own record mirrors D9 meanwhile: the same names, `u64` values, missing means
   unavailable, read once from the base commit. **To adjust once the values can be read from
   GolemDB's schedule:** `FeeScheduleSource` reads the pricing snapshot instead, the
   fee-schedule record leaves genesis (a genesis change), and `config.arkiv.tokenomics` moves
   into GolemDB's genesis model. Arkiv needs from GolemDB: an initial model with a host code
   version and the `host.arkiv.*` weights in its genesis config, opening the database with that
   host code version and weight names, and a call returning the snapshot's weights for a branch
   or commit, mirrored in Arkiv's `Store` adapter. Placeholder
  values in `genesis/dev.json` and `genesis/example.json`, including the
  `host.arkiv.max_lifetime` admission limit, fixed by the same block-base snapshot.
   *Done when:* the genesis hash changes with the values, the executor reads the configured
   schedule from the block's base commit (a test proves it), and it admits entity operations
   using the configured `max_lifetime`. Pricing with the configured `capacity_rate` is checked
   in step 6, where storage is first charged.
   Split in two commits:
   - **5a, the schedule in chain state.** *Done (2026-10-26).* `config.arkiv.tokenomics`
     (required) in the chain spec and both genesis files; the record layout and
     `FeeScheduleSource` in `arkiv-tokenomics` (`schedule_record`); `seed_genesis` writes the
     record on both paths (the node's genesis root and the provider's start-up seeding), so
     the genesis hash covers every value; `GolemStateView` reads it at its base commit, and
     `HostStateView` passes it through. A chain whose genesis has no `config.arkiv`, possible
     on the experimental stores, has no schedule: 5b makes priced operations unavailable
     there, never free.
   - **5b, the lifetime check.** *Done (2026-10-26).* The executor reads the schedule at the
     block's base and passes `max_lifetime` in `ExecEnv`; create and an extend that moves the
     expiry refuse an expiry beyond it, or permanent, with `ExpiryBeyondMaxLifetime(uint64
     target, uint64 currentBlock, uint64 maxLifetime)`, a business-rule revert. A node whose
     genesis has no fee schedule does not start. The end-to-end test
     `the_lifetime_bound_follows_the_genesis_file` proves the executor uses the genesis file's
     value.

6. **Charging and the pool.**
   Split in five substeps (decided 2026-10-27), each its own commit with CI green. The first
   two carry the priorities: exact storage gas from GolemDB's bytes, compute from
   `PlaceholderCost` plus GolemDB's costs, purge at 0, folded into one gas fee.
   - **6a. Operations on the branch.** Per-operation execution inside one frame per
     transaction, as described below, replacing the in-memory overlay; each create advances
     the minting nonce itself; estimates always discard their frame (decided 2026-10-27).
     Charging stays as today. One test changes: a create and a delete of the same entity in
     one batch now leave a deletion in the delta log for an entity the base never had,
     harmless for every consumer (GolemDB indexes itself), rather than nothing. The
     prerequisite for 6b: only writes on the branch return per-operation receipts and
     `#meta`. *Done (2026-10-27):* `apply_with_effects` opens the frame, stages each op on the
     branch, and merges it only for a successful batch whose `ExecEnv::keep_writes` is set
     (`fees.charge`); a revert, a probe or a store fault discards it. A probe's sender phase
     skips a zero value debit, so it creates no account for a caller without one. A store
     fault in mid-batch is tested in 6b1, where the shared test store can inject one.
   - **6b. Gas metering, one fee,** in two commits (decided 2026-10-27). `gas_used` is the
     breakdown's total, charged as today: base fee burned, tip to the beneficiary, no pool
     credit yet; purge gas 0.
     - **6b1, the meter and compute.** `intrinsic_gas`, today `max(initial, floor)`, splits
       into `tx_gas` (21,000 plus calldata) and `floor_gas` (EIP-7623), both in `ExecEnv`, and
       `finish` applies the floor once: `tx_gas` is added, not maxed. Each op's
       `PlaceholderCost` and every GolemDB receipt cost (entity reads and writes, minting
       nonce) enter the `GasMeter`; the running check before each op writes. The executor
       carries the block's `FeeSchedule` itself (`ArkivExecutor::with_schedule`), the cheapest
       way until GolemDB's pricing snapshot provides it; `ExecEnv::max_lifetime` stays, set
       from the same read. `ExecOutput` carries `gas: GasBreakdown` in place of `gas_used`.
       Failure charges per the table below, except that a business-rule revert pays its
       compute so far including GolemDB's costs, with storage 0: GolemDB charges rolled-back
       work (R6). Checked settlement. The shared priced test store (below) arrives here, so
       tests see GolemDB costs reach the meter; it can also fail a chosen call, for the test
       that a store fault in mid-batch closes the frame (deferred from 6a).
       *Done (2026-10-27).* The shared store is `arkiv_interfaces::store::priced::Priced`.
       The read of the caller's minting nonce before decoding is entity cost too:
       `ExecEnv::golemdb_cost_before` starts the batch's meter with it, and a decode failure
       pays it. Host-level tests run entity transactions through `arkiv_entity_transact` on a
       `HostStateView` over the priced store, checking that GolemDB's work reaches `gasUsed`,
       the sender's debit and the tip, for success, revert and probe. Unchanged: a gas limit
       below the floor halts with zero gas reported; only estimation probes reach it.
       **Settled in 6e:** the state layer added an op's own read and write receipts with checked
       arithmetic and reported an overflow as `CostOverflow`, which the executor saw as a store
       fault aborting the block, where the rule is an out-of-gas revert. The sum now saturates
       (`Receipt::accumulate`), and the executor's checked meter turns the saturated cost into
       out of gas.
     - **6b2, storage gas.** Per op, the size before from `get_entity_with_meta` (0 for a
       create), the size after from the write's `WriteDetails`, storage time by the pricing
       rules (create, extend, growing patch; otherwise 0), the running check after each op,
       and storage gas rounded once per transaction. Outcome: a successful transaction's
       `GasBreakdown` has non-zero compute and storage, purge 0. Shown by an executor test
       whose create asserts all three, the storage gas derived in its comment from the
       entity's bytes and lifetime (as the worked example derives 84,697), and a host test
       whose `gasUsed` is that breakdown's total. The split itself reaches receipts in 6c.
       *Done (2026-10-27).* No op computes a size after (decided 2026-10-27): a create and a
       patch buy the write's own change from its `WriteDetails` (net growth, so a shrink buys
       0 and the size before cancels out), and an extend buys the size before, from the
       `#meta` of its ownership read (`get_entity_with_meta`, the same single call), for the
       added blocks; its own write changes no size, the expiry being a fixed-width cell. So
       `#meta` and `WriteDetails` are never combined, and a disagreement between them cannot
       produce a negative size; a wrong `#meta` would misprice an extend silently, which is
       for GolemDB's size conformance to catch. Storage time that does not fit is out of gas,
       like a cost. The running check after each op was already in place (`finish` includes
       storage gas). Executor tests cover a create (compute, storage 1,459, purge 0), a
       growing and a shrinking patch, an extend (also after a growing patch in the same
       batch, which buys the grown size), a transfer and a delete buying nothing, rounding
       once per batch, and storage running a batch out of gas. After review: a patch that
       removes and adds (an equal swap buys 0, a net growth its growth), a create with an
       indexed and an unindexed attribute (index bytes and `c_idx` priced), and storage gas
       beyond `u64` as out of gas with nothing kept.
       **Open, for step 6's done-when:** every figure is tested on MemStore only, which agrees
       with Valkey by the shared `WriteDetails` conformance suite; no test prices on a real
       GolemDB engine yet (the `arkiv-golemdb` dev-dependency above). `tests/backend_neutral.rs`
       likewise runs on MemStore only, despite its name. *Mostly closed in 6e:* e2e tests price
       on a real GolemDB engine through the pool's balance, each figure derived from the size
       rule: a create with an indexed attribute (`the_pool_receives_the_storage_share_and_the_value`),
       a growing patch and an extend (`a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`,
       `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`, which also has a
       shrinking patch), and a create whose storage does not fit
       (`storage_that_does_not_fit_runs_a_create_out_of_gas`). Only the executor-level receipt
       test on a GolemDB-backed view, under step 6's done-when, remains open.
   - **6c. Fee estimate and receipt fields:** *postponed to the target tokenomics (decided
     2026-10-27).* One fee from outside is a conscious shortcut: `eth_estimateGas` and
     `gasUsed` already include storage, since they run the same executor. `arkiv_estimateFee`
     and the receipt fields stay in the concept ([api-spec](api-spec.md#arkiv_estimatefee));
     they read the breakdown the executor already returns.
   - **6d. `$rate`:** the cell in the record layout and its update per operation.
     *Postponed (decided 2026-10-27),* with the pool's payouts to validators, which are out of
     scope anyway: nothing in iteration 1 reads `$rate`; it serves exact payouts and refunds.
     The design below stays the one to start from. The cost of waiting: entities created
     before it exist have no record of the price they paid, and adding the cell then changes
     the record layout on a running chain, so it needs a rule for records without it (or a
     migration) rather than a genesis change. Clean while iteration 1 runs on dev and test
     chains only.
   - **6e. The pool:** `ARKIV_ADDRESS` as an account credited with `base_fee ×
     storage_gas` and the value of a successful call, the money-conservation identities.
     *Done (2026-10-27).* Value is not rejected (decided 2026-10-27): a call to
     `ARKIV_ADDRESS` that succeeds moves its value to the pool, as a payable contract keeps what
     it is sent; a revert keeps it with the sender, and a probe (`eth_call`,
     `eth_estimateGas`) moves none, like its nonce. Mined view calls follow the same rule. The
     target tokenomics builds on this rather than replacing it: it pays storage and purge out of
     the value and returns the change. Faucet and other plain transfers are unaffected. With
     nothing rejected, no transaction-pool rule is needed. `credit_pool` credits `value +
     base_fee × storage_gas` after `charge_sender`, on charged transactions only; a zero credit
     touches nothing, so the pool's account comes into existence with its first credit and
     has no genesis entry. Since the pool also holds gifts, its balance is at least, not
     exactly, the sum of its storage credits. The state layer's receipt sum saturates (see 6b1).
     Host tests: the money identity (sender debit = burn + tip + pool credit) for a create, a
     revert and a view call, value to the pool on success only, and a probe moving nothing.
     The e2e test `the_pool_receives_the_storage_share_and_the_value` runs a create with an
     indexed attribute and value on the in-process store and on GolemDB: the pool rises by
     `17 × base_fee + value`, which also prices storage on a real GolemDB engine (6b2's open
     note). `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas` does the same
     for a create, a growing patch and a one-month extend, each its own transaction with a
     predicted key, and prints each fee's split (`--no-capture`).
     After review: mined view calls now get the sender checks before their first write, as
     entity transactions and transfers do; they had none, so a sender who covered the value
     but not the gas left a debit and a nonce bump on the branch behind the error (predating
     6e; 6e's value made it worse). Credits saturate, the pool's like every other, which
     loses nothing while the total supply fits `U256`; genesis does not check that its
     allocations do, a possible follow-up. New tests: such a view call leaves the branch
     untouched, and a decode failure and an out-of-gas batch keep their value like a revert,
     with no pool account on the branch (read by its record key, not inferred from a zero
     balance). Second review: a view call or a plain transfer whose gas limit is below its
     flat charge now halts out of gas before any write; the sender checks bound the charge
     by the limit, so settling more than the limit could clamp behind the error. The pool
     rejects such a limit at ingress, but the executor no longer relies on that.
   - **The `$rate` cell enters the record layout** (moved from step 3; postponed with 6d), before any create is
     priced, so measured units include it from the first create: `SYSTEM_CELLS` goes from 7 to
     8 (`entity_records.rs` 54), so `MAX_RECORD_CELLS` becomes 40 with 32 attributes, and both
     genesis files raise `record_limits.max_cells` from 39 to 40 (`max_indexed_cells` stays 36:
     `$rate` is not indexed). The genesis hashes change. How `$rate` maps between `Entity` and
     the record is open (Decision 6). Tests: a create and a patch at the maximum number of
     attributes still fit.
   - `ExecEnv` gains `base_fee` (from `FeeEnv`), for the pool's share and
     `$rate`.
   - **Per-operation execution on the branch.** `apply_with_effects`
     (`arkiv-reth-executor/src/arkiv.rs`) today stages every operation in an in-memory overlay
     and writes one net change per entity after all succeed (118–150). It becomes: open one
     frame for the transaction; per operation, in order,
     1. add its `arkiv_op_cost` and check `compute_so_far ≤ gas_supplied` before writing;
      for create and extend, apply the `max_lifetime` admission check above before any
      operation writes or storage-time arithmetic;
     2. run its record calls on the branch (ownership read, write, `$rate` write), adding each
        receipt's cost and the operation's `storage_time`;
     3. check `max(compute_so_far, floor_gas) + ceil(storage_time_so_far × capacity_rate /
        10⁶) ≤ gas_supplied`: the floor applies to compute, so the running check includes it.
     After the last operation, `finish` the meter and check the final `gas_used ≤
     gas_supplied` before keeping the frame. Any failure discards the frame. Reads see earlier
     operations through the branch (`ViewWithOverlay`), so the overlay is no longer needed for
     that. Each create advances the minting nonce as part of the operation, inside the frame,
     so its receipts include the nonce's read and write. The value it returns is the one the
     decoder derived the create's key from by construction: the decoder reads the start
     nonce from the same view, just before the batch runs. `ExecEnv` gains
     `base_fee`, `tx_gas` and `floor_gas` (`intrinsic_gas`, `lib.rs` 152–159, merges the last
     two today).
   - **The meter lives outside the frame** (step 2b's rule). Each operation's entity receipts,
     through the 2c trait methods, are added to the executor's meter as they arrive, so a
     discarded frame loses its writes but keeps what they cost. `Outcome` carries the summed
     entity receipt (formerly step 2d).
   - **A shared priced test store.** `metering_tests.rs`'s `Priced` wrapper moves into
     `arkiv-interfaces` (behind `conformance`) and implements `StoreExt`, so executor tests
     run over a `HostStateView` with non-zero costs; that also covers `HostStateView`
     forwarding receipts, which zero-cost MemStore tests cannot see.
   - **Charges on failure** (proposed):

     | Failure | `gas_used` |
     | --- | --- |
     | compute out of gas | `gas_supplied`, as in the EVM and as today (`arkiv.rs` 125) |
     | storage out of gas | `gas_supplied` |
     | arithmetic overflow | `gas_supplied`; an `OutOfGas` revert, never an `Err`, which would abort building the block |
     | business-rule revert | `min(max(tx_gas + Σ op costs including the failing one + Σ GolemDB costs spent, floor_gas), gas_supplied)`; storage and purge 0 (decided 2026-10-27: GolemDB charges rolled-back work) |

     `gas_supplied ≥ floor_gas` is already checked (`lib.rs` 821). The spec's "compute spent up
     to the revert" needs this qualifier for out-of-gas.
    A lifetime-limit violation is a business-rule revert, not an out-of-gas failure.
   - **Estimates run isolated.** reth's `eth_estimateGas` calls `transact` once per probe on one
     branch (`lib.rs` 351, 843), so a probe would see an earlier probe's growth or extension and
     price its storage at 0. With fee charging off (`!fees.charge`), the transaction frame is
     always discarded. An e2e test estimates a growing patch and an extend.
   - **`ExecOutput`** carries the `GasBreakdown`. In `arkiv-reth-executor/src/lib.rs`, the
     transaction terms are added rather than maxed (`tx_gas` with `floor_gas` applied once, in
     `finish`, replacing `outcome.gas_used.max(floor)`), and `gas_used = compute_gas +
     storage_gas`. This applies to entity transactions only; the purge (`lib.rs` 778) and plain
     transfers (691) keep their `max`.
   - **Tests that change:** the out-of-gas test at `arkiv.rs` 1034, every gas figure once
     `tx_gas` is added, and `genesis_alloc_has_dev_funding_only`.
   - Value on calls to `ARKIV_ADDRESS` goes to the pool when the call succeeds (revised
     2026-10-27, was rejected; see 6e). Before 6e the executor ignored it.
   - The pool is the **Arkiv account at `ARKIV_ADDRESS`**: the address entity
     transactions are sent to becomes an account with a balance (nonce 0, no
     code). `charge_sender` credits it `base_fee × storage_gas`; the rest of the
     base fee is burned and the priority fee is paid as today. Its balance is
     visible through `eth_getBalance(ARKIV_ADDRESS)`. It has no genesis entry (revised
     2026-10-27): the first credit creates its account, so `arkiv-genesis`'s test
     `genesis_alloc_has_dev_funding_only` still holds.
    - Carry the shared breakdown through settlement. Assert the gas-sum and
       money-conservation identities above. Zero the storage/purge components and pool
       credits on revert; purge is zero on success as well.
   *Status (2026-10-27):* the e2e part is met, each test on the in-process store and on
   GolemDB with a printed fee table: `the_pool_receives_the_storage_share_and_the_value`,
   `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`,
   `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage` (each growing op estimated
   twice to the same figure and sent with that estimate as its limit; the shrink buys 0) and
   `storage_that_does_not_fit_runs_a_create_out_of_gas` (all gas paid, no entity, no mint,
   no pool credit). The conservation identities are host tests. Still open: the
   executor-level test on a GolemDB-backed view below.
   *Done when:* e2e tests show a create's `gasUsed` including the storage
   part, the balance of `ARKIV_ADDRESS` rising by exactly `base_fee × storage_gas`,
   an extend charging only the added blocks, a shrinking patch charging and
   refunding nothing, and a batch whose storage gas does not fit reverting with
   `OutOfGas` and leaving no entity behind, and the value of an entity transaction
   reaching the pool. The breakdown's sum must equal `gasUsed`, and sender debit
   must equal burn plus proposer payment plus pool credit, including tests with nonzero
   priority fees and reverted batches.
  Admission tests cover creates and extensions exactly at `max_lifetime` and one block
  beyond, including an extension whose added blocks fit but whose total remaining lifetime
  exceeds the limit. Over-limit and permanent-expiry attempts must return the typed revert,
  roll back earlier operations in the batch, and retain no storage or purge purchase;
  estimates must report the same revert.
   An executor-level test on a GolemDB-backed view (a new `arkiv-golemdb` dev-dependency of
   `arkiv-reth-executor`) sees per-operation receipts that include the ownership read, the
   entity writes and the minting nonce, with exact byte details, exclude balance and
   transaction-nonce traffic, and keep what a discarded frame spent.

7. **`arkiv_estimateFee`.** *Postponed to the target tokenomics (decided 2026-10-27);* kept
   here as the design to start from. Until then clients use `eth_estimateGas`.
   The RPC method of the [Fee API](api-spec.md#arkiv_estimatefee), with iteration 1's values:
   `gasLimit` = `compute_gas + storage_gas + purge_gas`, `requiredValue` = 0;
   expose `computeGas`, `storageGas`, `purgeGas` and the same pool-credit fee fields as
   receipts. `computeFee = effective_gas_price × compute_gas` and
   `totalFee = effective_gas_price × gasLimit`. Do not add `storageFee` or `purgeFee`
   to that total: they are already funded by gas. Equivalently,
   `totalFee = computeFee + storageFee + purgeFee + priority_fee × (storageGas + purgeGas)`.
   This overrides the target API's `computeFee` and `totalFee` relationships for iteration 1.
   Purge fields remain zero. It runs the same executor as inclusion on a disposable
   branch; the SDK signs `gasLimit` and sends exactly zero `value` in this iteration.
   *Done when:* for a create, extend and patch, `gasLimit` equals the
   `gasUsed` of inclusion at the same state, schedule and execution height; every component
   matches the receipt, and the total fee reconciles with settlement at the quoted gas price.
   A reverting batch returns error 3 with the typed revert. Test nonzero priority fees and
   re-estimation at a different height; a quote is not a guarantee for later inclusion.

8. **Integration check.** *Pending.*
   Run the SDK's network-health suite against a local node on GolemDB.
   `eth_estimateGas` includes storage gas automatically, since it runs the
   same executor. Record what changes for clients (gas figures).

Steps 4 and 5 do not depend on GolemDB and can run in parallel with 2–3.

## Decisions

Decided (2026-10-23):

1. **Pool location:** an account, with nonce and balance, at a constant
   address. Revised 2026-10-25: the pool is the account at `ARKIV_ADDRESS`
   itself, the address entity transactions are sent to; no second address.
2. **Storage units:** write first, read `#meta`, and roll back if out of gas.
   Revised 2026-10-26: measured per operation on the branch, from the ownership read's
   `#meta` and the write receipt, inside one transaction frame.
3. **Gas size:** storage and compute fold into a single gas fee in this
   iteration; storage and purge paid through `value` at `min_base_fee`, as in
   the spec, is a later iteration.

Decided (2026-10-25):

4. **All fees through gas,** with the [known limitations](#known-limitations).
   Non-zero `value` on entity transactions is rejected. Revised 2026-10-27: not rejected;
   the value of a successful call goes to the pool, as to a payable contract.
5. **`arkiv_estimateFee` in iteration 1,** so the SDK handles the later switch
   through the same quote-and-sign workflow, with fresh quotes at activation.
   Revised 2026-10-27: postponed, with the receipt fields, to the target tokenomics. One fee
   from outside is a conscious shortcut, so iteration 1 changes no Ethereum mechanics and
   clients use `eth_estimateGas` and `gasUsed`; what matters now is the internal split of
   compute, storage and purge, which the switch then only has to expose.

Decided (2026-10-26):

- **Storage is priced per operation,** in batch order; a batch costs what the same operations
  cost in sequence, sharing only `tx_gas`.
- **All costs accumulate in memory while the record calls run on the branch,** inside one
  transaction safe point.
- **`max_lifetime` is an entity-operation admission check in iteration 1.** Creates and
  extensions beyond the total remaining-lifetime bound revert; permanent expiry has no
  exemption and is explicitly rejected.
- **Minting-nonce traffic is entity cost** (`golemdb_cost`).
- **`max_lifetime` is a field of `FeeSchedule`,** with no default: the five weights have
  placeholder defaults, but a schedule cannot be built without choosing a lifetime, and step 5
  stores and reads one record.
- **A patch pays for its net growth:** `units_after − units_before` from the write's
  `WriteDetails`, 0 when negative, not per grown cell, so swapping an attribute for another of
  the same size buys nothing.
- **`config.arkiv.tokenomics` is required** wherever `config.arkiv` is present: a chain
  without a fee schedule could price nothing, so it does not start. That holds on every
  store: a genesis without `config.arkiv` stops the node too, so at runtime a missing
  schedule is a store fault, never an operation priced as free.
- **Compute keeps `PlaceholderCost` until GolemDB reports real costs** (decided 2026-10-27).
  GolemDB's metering is progressing in parallel but still reports 0, so compute is
  `tx_gas + PlaceholderCost + golemdb_cost` as in the pricing rules. Once GolemDB's costs
  are real, `PlaceholderCost` is dropped or recalibrated, so the same work is not charged
  twice.
- **Purge gas is metered as 0 for now** (decided 2026-10-27): the field and plumbing exist,
  but no deletion reserve is collected. Lower priority for the coming weeks, as the first
  nodes run as archival nodes; the exact bound needs GolemDB's deletion-cost estimate (its
  iteration 7).
- **The lifetime bound is a business rule,** checked by the executor when it stages a create
  or an extend that moves the expiry, not by the decoder: the batch reverts with
  `ExpiryBeyondMaxLifetime(uint64 target, uint64 currentBlock, uint64 maxLifetime)` and pays
  for the operations it ran. An extend that keeps the expiry stays a no-op.
- **`max_lifetime` starts at 7,884,000 blocks,** six months at 2-second blocks, in both genesis
  files.
- **Integer widths.** Gas is `u64`. Per-operation and per-transaction wei amounts computed in
  `arkiv-tokenomics`, `$rate` and its intermediates included, are checked `u128`, so the crate
  needs no big-integer dependency. Balances and pool accounting (sender debit, pool credit,
  burn, tip, and later `R`, `L` and payouts) are `U256`.

**Accounting clarification (2026-10-25).** Gas-only payment is retained. Compute, storage
and purge are separate accounting components in iteration 1, including receipt and estimate
fields. Purge-reserve collection remains deferred, so its fields are zero. Aggregate pool
liabilities (`R` / `L`) and validator payouts remain out of scope; a per-transaction split
does not imply full pool accounting.

Still open:

6. **`$rate`'s spelling** (postponed with 6d)**:** `$rate` as a field (not queryable), added to `annotations` and
   to the API spec's system attributes and reserved-name rules. The fee-schedule record key
   is decided (2026-10-26): `keccak256("arkiv.fee_schedule")`, for the interim record of
   step 5. Account keys start with 12 zero bytes and entity keys are hashes of other inputs,
   so neither can collide with it.
7. **Placeholder values,** chiefly `capacity_rate`, starting at the spec's `min_base_fee`
   10 gwei and `capacity_rate` 14: storage gas must fit the
   dev chain's 30M block gas limit and the EIP-7825 per-transaction cap for
   realistic entities and lifetimes.

## Coordination

- The adapter merged with PR #222 dropped receipts on purpose; `d0f48a4` maps them and
  step 2 passes them on. Worth a heads-up to its author.
- GolemDB's write pricing (its iterations 4–6) will replace cost 0 with real
  costs, and its iteration 5 will persist `host.*` weights at genesis. Step
  5's `FeeScheduleSource` is the seam for that switch.
