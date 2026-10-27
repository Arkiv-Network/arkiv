# Arkiv Tokenomics

Status: draft, 2026-10-25. Defines the fees, payment rules and resource limits of Arkiv.
[Tokenomics Execution](tokenomics-execution.md) specifies execution and accounting mechanics;
the [Fee API](api-spec.md#7-fee-api-draft) specifies RPC fields, defaults and errors.
[GolemDB Metering](golemdb-metering.md) supplies work and size measurements. Where the
execution document differs, this spec takes precedence. Unresolved rules are listed below.

## Contents

- [Summary](#summary) · [Principles](#principles)
- [The Fee](#the-fee): [Compute](#compute) · [Storage](#storage) · [Purge](#purge) ·
  [Levers](#levers)
- [Paying the Fee](#paying-the-fee) · [Where the Money Goes](#where-the-money-goes)
- [Limits](#limits) · [Fee Schedule](#fee-schedule)
- [Block Processing](#block-processing) · [Expiry and Purge](#expiry-and-purge)
- [Estimation](#estimation)
- [Example](#example) · [Price Levels](#price-levels)
- [Open Decisions](#open-decisions) · [Out of Scope](#out-of-scope)

## Summary

The sender prepays three costs: executing an operation, storing its data for a chosen lifetime,
and deleting it at expiry. **Gas pays compute at a congestion price; transaction `value` pays
storage and purge at an admin-set price.** Storage payments fund a protocol-controlled pool
intended to pay nodes while they hold the data. The first version makes no payouts.

## Principles

1. **Separate resource prices.** Busy blocks must not raise storage prices, nor full disks
  raise compute prices.
2. **Collect upfront; do not refund purchases.** Shrinking, deleting or shortening a lifetime
  returns no prepaid fee. Unspent transaction value is change, not a refund.
3. **Pay the nodes bearing the cost.** Storage fees belong to the pool, not the current proposer.
4. **Honour existing purchases.** Price changes affect new purchases; capacity and lifetime
  limits bound the commitments the protocol sells.

## The Fee

```
fee = (base_fee + priority_fee) × compute_gas                 congestion-priced, paid in gas
    + min_base_fee × (storage_gas + purge_gas)                 fixed-priced, paid through value

compute_gas = max(tx_gas + Σ (arkiv_op_cost(op) + golemdb_cost(op)), floor_gas)
storage_gas = ceil(Σ storage_time(op) × capacity_rate / 1_000_000)
purge_gas   = Σ purge_bound(op)

storage_time(op) = storage_units(op) × blocks(op)                  per operation: Storage
storage_units    = cell_bytes × cell_byte_units + index_bytes × index_byte_units
                 + cells × c_cell + indexed_cells × c_idx              (counts from #meta)

tx_gas       = 21_000 + 4 × zero_bytes + 16 × non_zero_bytes        Ethereum, EIP-2028
floor_gas    = 21_000 + 10 × (zero_bytes + 4 × non_zero_bytes)      EIP-7623
priority_fee = min(max_priority_fee_per_gas, max_fee_per_gas − base_fee)
```

Sums run over the batch's operations. Bytes are the transaction's calldata. All `_gas` and
`_cost` terms are in gas; on Arkiv one GolemDB cost unit is one gas.

### Compute

`compute_gas` is the work in the including block. It is the transaction's gas: it counts against
the block gas limit, drives the EIP-1559 base fee and pays the priority fee.

- `tx_gas` follows EIP-2028 and `floor_gas` EIP-7623, consistent with reth's pool admission.
  The floor applies to compute only.
- `arkiv_op_cost` covers stateless decoding and validation; `golemdb_cost` includes every
  database call, including authorization reads. Weights are calibrated so a reference node
  processes a full block within the block time.
- `base_fee` follows EIP-1559 on compute gas, with a floor of `min_base_fee`.
- Non-empty access or authorization lists, and type-3 or type-4 transactions, are rejected
  with typed errors; Arkiv has no EVM execution for them.

### Storage

`storage_gas` is the storage the batch buys, for the blocks it buys it.

| Operation | `storage_units(op)`: units bought | `blocks(op)`: blocks bought | `purge_bound(op)` |
| --- | --- | --- | --- |
| create | the record's `storage_units` | `expires_at − block_number` | the record's D5 deletion bound |
| extend | the record's `storage_units` | `new_expires_at − old_expires_at` | 0 |
| patch that grows | the increase of `storage_units` | `expires_at − block_number` | the bound's increase, if any |
| patch that shrinks or keeps size | 0, no refund | – | the bound's increase, if any |
| delete by the owner | 0, no refund | – | 0, no refund |

- **`capacity_rate`** is the price of storage in micro-gas per unit-block (one storage unit for
  one block), so a unit-block costs `min_base_fee × capacity_rate / 1_000_000` wei.
- **A batch is priced as its operations in sequence.** Each operation buys storage as in the
  table, in batch order; only `tx_gas` is shared, and `storage_gas` is rounded once over the
  batch. The sender controls the order, and with it the cost.
- **Lifetime is bought, not rented.** Shortening it, shrinking or deleting returns nothing;
  applications buy short lifetimes and extend.
- **Live residency only.** GolemDB's write weights already pay for its short history retention
  (`#minRetention`), so `storage_time` runs from the write to expiry.

### Purge

The D5 deletion bound is prepaid at create and topped up when a patch increases it.
At purge, the pool burns the actual deletion cost at the then-active `min_base_fee`.
The difference between collected reserve and actual charge stays in, or comes from, surplus.
An owner's early delete instead pays compute gas and leaves the reserve in the pool.

### Levers

| Lever | Concern | Applies to |
| --- | --- | --- |
| GolemDB weights, `arkiv_op_cost` | calibration of work | compute, purge |
| `cell_byte_units`, `index_byte_units`, `c_cell`, `c_idx` | calibration of disk | storage units |
| block gas limit | throughput | compute |
| `capacity_rate` | storage capacity | storage |
| `min_base_fee` | token price | compute floor, storage, purge |
| `base_fee` | congestion, automatic | compute |
| `purge_budget` | purge throughput | purge |

Calibration changes require measurements. The admin adjusts `capacity_rate` against disk
utilization and `min_base_fee` against token price; for example, doubling the latter offsets
a halving of the token price for new storage purchases. The adjustment policy remains open.

## Paying the Fee

An entity transaction calls `ARKIV_ADDRESS`, a payable system contract whose balance is the
storage pool. Its operations form one atomic batch.

```
gas_used            = compute_gas                            ≤ gas_limit
effective_gas_price = base_fee + priority_fee                standard EIP-1559
storage_fee         = min_base_fee × storage_gas
purge_fee           = min_base_fee × purge_gas
change              = value − storage_fee − purge_fee        ≥ 0, else revert
```

**Before inclusion**, the sender must cover `value + gas_limit × max_fee_per_gas` and pass
envelope and nonce validation. Invalid transactions are not included and pay nothing.

**On success**, the full value moves to the pool, the storage and purge fees remain there,
and change returns in the same transaction. Compute is settled at `effective_gas_price` and
the unused gas reservation is released. Storage gas is rounded up once over the entire batch.

**On revert**, entity writes, the value transfer and logs roll back; the nonce is consumed
and compute spent up to the failure is charged. No storage or purge fee is paid. Too little
value produces `InsufficientValue(required, provided)`.

Receipts retain Ethereum's `gasUsed` and `effectiveGasPrice` and add `storageGas`, `purgeGas`,
`storageFee` and `purgeFee`. Successful nonzero transfers emit `ValueReceived` first and
`ValueReturned` last. Their signatures and field mappings are in the
[Fee API](api-spec.md#receipts-and-fee-logs); checkpoint and settlement rules are in
[Transaction Lifecycle](tokenomics-execution.md#transaction-lifecycle).

## Where the Money Goes

| Part | Amount | Goes to |
| --- | --- | --- |
| Compute | `base_fee × compute_gas` | burned, as in EIP-1559 |
| Priority fee | `priority_fee × compute_gas` | block proposer |
| Storage | `storage_fee` | storage pool at `ARKIV_ADDRESS` |
| Purge reserve | `purge_fee` | storage pool at `ARKIV_ADDRESS`, burned when the purge runs |
| Change | `value − storage_fee − purge_fee` | back to the sender |

**Pool accounting.** Only protocol rules move the pool's funds; nobody holds its key.
Each entity stores `$rate`, its prepaid wei per block, so later prices cannot rewrite earlier
purchases. Full accounting tracks:

- `R`: the sum of live entities' rates, owed per block.
- `L`: the sum of their rates times remaining lifetimes, owed in future blocks.
- Solvency: the balance must cover `L` plus outstanding purge work. The project seeds the
  pool at genesis; unused storage, unused reserves and rounding contribute to surplus.

Rate changes replace the old remaining liability with the new rounded-rate liability;
rounding remainders go to surplus, not `L`. Shrinking or deleting releases unused liability
to surplus. Expiry ends storage payment regardless of when physical deletion happens; O17
keeps open how the delay until purge is paid for.
[Pool Accounting](tokenomics-execution.md#pool-accounting) defines the per-operation updates,
rounding example and deferred expiry-ledger proposal. For its formulas,
`storage_price = min_base_fee × capacity_rate / 1_000_000` and
`added_fee = storage_price × storage_units(op) × blocks(op)`, before transaction-level rounding.

**First version:** store `$rate` and pool totals, but make no payouts or on-chain solvency
check. Exact first-version expiry accounting is deferred: without a ledger, `R` lags expiry
until purge and decrementing `L` by that rate can produce negative liability. Monitoring does
not resolve this missing consensus rule. The insufficient-funds policy also remains open.
Future payouts must respect the recorded purchase rates; validator distribution is outside
this spec. Rates fix token amounts, not fiat value, with exposure bounded by `max_lifetime`.

## Limits

These schedule parameters bound purchases and decide block validity:

- **`max_lifetime`:** the furthest ahead a create or extend may set the expiry.
- **`max_block_storage_units`:** new storage units one block may sell.
- **`max_live_bytes`:** the raw bytes of all entities not yet purged. The admin raises it as
  node disks grow.
- **`purge_budget`:** the gas the purge may spend per block, outside the block gas limit
  ([Expiry and Purge](#expiry-and-purge)). A protocol parameter that may change at runtime:
  it lives in the fee schedule, changes through the admin path like any schedule entry, and
  applies per block from that block's snapshot, identically on every node. Starting value
  6,000,000 gas, 20% of the 30M block gas limit.

The builder leaves transactions that do not fit the remaining storage capacity for a later
block. Creates can be checked before execution; growing patches may need execution before
being dropped, unpaid. A transaction exceeding `max_block_storage_units` on its own instead
reverts and pays compute. When limits bind, selection is by priority fee, as for gas.
Limits hold after every included transaction. `live_bytes` uses raw `#meta` cell and index
bytes, not weighted storage units, so recalibration does not change the byte count.

## Fee Schedule

Arkiv's weights, `min_base_fee` and limits live in GolemDB's schedule as `host.*` entries
(metering D9). All fees in a block use one snapshot of both sets of weights, captured at the
branch base. GolemDB stores the host entries; Arkiv prices with them and enforces its limits.
A missing required weight makes an operation unavailable, not free.

One permissioned admin transaction installs changes, announced at least `#minActivationDelay`
blocks ahead. During calibration, immediate patches are also allowed; their precise activation
boundary still needs definition consistent with the one-snapshot rule. Signed gas and value
caps limit the sender's charge, but a change can make the transaction revert.
`arkiv_getFeeSchedule` returns the active and any pending schedule.

## Block Processing

1. **Purge.** A system step, not a transaction: it deletes expired entities within
   `purge_budget` ([Expiry and Purge](#expiry-and-purge)).
2. **User transactions.** Each pays as in [Paying the Fee](#paying-the-fee). Only compute gas
   counts against the block gas limit, and the storage limits hold after every transaction
   ([Limits](#limits)). A transaction that fails is included and pays the compute gas it used.
3. **End of block.** The `arkiv_state` record is written once; the header's `gasUsed` is the
   block's compute gas and sets the next base fee.

Validators re-execute all three steps and reject the block if any result differs.

## Expiry and Purge

**Expiry is immediate, deletion is not.** At its `$expiresAt` block an entity becomes invisible
to queries and `get` and accepts no operation: it answers `NotFound`, as if it never existed.
Deleting it from state is a separate step, the purge, which may run later. Until then its bytes
still count in `live_bytes`, since nodes still hold them.

**Budget.** Purge work, including selection reads and failed attempts, consumes up to
`purge_budget` per block, outside user gas. It never feeds the base fee, and unused budget is not
given to users. Nodes must support the block gas limit plus this budget. The budget is fixed
within a block and adjustable between blocks ([Limits](#limits)):

- **Sizing.** Purge work follows creates, not all operations: patches, extends and transfers use
  block space without adding purge work, and owner deletes remove it. Purging an entity costs
  about 0.6× creating it, so the budget needed is about 0.66 × the gas spent on creates, plus a
  reserve to catch up after expiry waves. 20% of the block gas limit, about 55 purges per block
  for 1 KiB entities, covers a sustained mix in which up to about 30% of the gas at the EIP-1559
  target goes to creates, with a 2× reserve.
- **Floor.** The budget must cover the largest possible deletion plus its selection read, or
  such a record would stop the purge for good: the D5 bound at the record caps, about 930,000
  gas at the dev genesis caps (39 cells, 36 indexed) and the example's weights.
- **Adjustment.** A backlog only delays deletion: expired entities are already invisible, and
  only their bytes keep counting in `live_bytes`. The admin raises the budget when the distance
  from `purge_cursor` to the current block keeps growing.

**Selection.** The draft [purge algorithm](tokenomics-execution.md#purge) queries the parent's
committed expiry index, oldest term first, then ascending record ID. It uses a protocol-fixed
page size of 256 and one query per term per block. A complete short page, fully deleted,
advances `purge_cursor`; a full page or budget failure leaves it on the current term.
Completed deletions survive a later budget failure. The execution document specifies exact
query parameters, exhaustion tests and cursor updates; nodes must not choose local variants.

Selection and deletion must be deterministic so validators derive the same state root without
a purge list in the block. Entity keys never repeat. Deleted bytes leave `live_bytes`; the
pool pays the burn described above.

**Progress is not yet guaranteed.** The budget must cover bounded selection work plus a
maximum deletion. A 256-key limit does not bound query work, and the one-page rule limits
throughput per expiry term. Read metering and adequate budget/throughput remain prerequisites.

## Estimation

Both estimates use the inclusion executor on a disposable branch above the requested block,
without committing. GolemDB calls use `Unlimited` budgets; the requested `gas` or node cap
still bounds estimated compute. The quote prices for the following block.

| Method | Returns | Funding behavior |
| --- | --- | --- |
| `arkiv_estimateFee` | `gasLimit`, `requiredValue`, and the fee split | Ignores supplied value and sender balance; other inclusion reverts still fail |
| `eth_estimateGas` | Compute gas alone | Requires sufficient value and normal funding; low value returns `InsufficientValue` |

For the same successful execution context, both return the same compute gas. The Arkiv method
accepts `[transaction, block?, stateOverride?]`; the [Fee API](api-spec.md#arkiv_estimatefee)
defines all request/response fields, defaults and errors.

**Validity.** Gas and required value are exact for a given transaction, state, schedule and
execution block context, not a promise for later inclusion. Height alone changes the remaining
lifetime charged by a growing patch; expiry can make an otherwise unchanged batch revert.
State or schedule changes can also invalidate the quote. `pendingScheduleAt` warns only about
announced schedule changes. Re-estimate when the target block changes, especially near expiry.

**Buffers.** Base-fee changes affect compute's price, not its gas count or storage/purge fees;
`max_fee_per_gas` covers compute price headroom. The SDK may buffer `requiredValue`, but this
does not guarantee sufficient funding or validity in a later block.

## Example

A 1 KiB entity has 1,556 storage units and a 105,000-gas deletion bound. Create it for
3,888,000 blocks (90 days at 2 seconds), with `capacity_rate = 14`, `min_base_fee = 10 gwei`,
base fee 20 gwei and priority fee 1 gwei:

| Part | Calculation | Charge (wei) |
| --- | --- | --- |
| Compute | `(38,088 + 10,000 + 140,000) gas × 21 gwei` | 3,949,848,000,000,000 |
| Storage | `ceil(1,556 × 3,888,000 × 14 / 1,000,000) = 84,697 gas`, at 10 gwei | 846,970,000,000,000 |
| Purge | `105,000 gas × 10 gwei` | 1,050,000,000,000,000 |

The sender needs 188,088 compute gas and at least 1,896,970,000,000,000 wei of value.
The total fee is 5,846,818,000,000,000 wei, about $0.00058 at GLM $0.10. A higher base fee
changes only the compute charge. The [execution example](tokenomics-execution.md#example-the-pool-over-one-entitys-life)
continues through growth, extension across a price rise, and purge.

## Price Levels

Proposed starting values are `min_base_fee = 10 gwei` and `capacity_rate = 14`.
At GLM $0.10 and 1,296,000 blocks per 30 days, these price a GiB of storage units at about
$20 per month, excluding compute and purge. They are calibration proposals, not fixed protocol
constants or a guarantee of node profitability.

## Open Decisions

| ID | Question | Current view |
| --- | --- | --- |
| O2 | What happens to a pool surplus well above liability: burned, released to storers, or used to lower `capacity_rate`? | open |
| O3 | The rule the admin follows to adjust `capacity_rate` (storage usage against capacity) and `min_base_fee` (token price) | open |
| O6 | Value of `purge_budget`, and the rule for adjusting it | open; starting value 6,000,000 gas, 20% of the block gas limit, raised by the admin when the purge backlog keeps growing ([Expiry and Purge](#expiry-and-purge)) |
| O9 | Values of all weights and limits, the pool's genesis seed, and who may change them | open; starting point `min_base_fee` 10 gwei and `capacity_rate` 14 ([Price Levels](#price-levels)) |
| O10 | A log per purged entity, so indexers learn removals without re-executing | proposed, informational only |
| O11 | Keep the EIP-7623 floor on a data chain without blobs? | kept for now: reth's pool enforces it |
| O16 | Read metering: the owner `get` behind patch, extend and delete, and the purge query, enter consensus gas | prerequisite: GolemDB must specify read metering before fees go live |
| O17 | Storage between expiry and purge: nodes still hold an expired entity's bytes until it is purged, but its payment ends at `$expiresAt`. Paying `R` until purge without funding would spend other entities' prepayments and drive `L` negative ([Pool Accounting](tokenomics-execution.md#pool-accounting)) | open. Options: (a) expiry ends payment, as today's rule, and nodes carry the gap, kept short by `purge_budget`; (b) prefinance at purchase: each purchase also pays for an expected delay past expiry, so `$rate` leaves `R` at purge with `L` covered; open how the delay is sized (fixed blocks, or from `purge_budget` and the observed backlog) and what a later purge does; (c) release less per block than collected: the pool pays out a share of `R`, and the remainder funds the gap or accumulates as protocol fees |

Review follow-ups remain open: first-version expiry accounting and the bucket-ledger proposal;
a policy for purge charges exceeding the pool balance; and the activation boundary for immediate
schedule patches. Pagination assumes complete, deterministic query pages; bounded read work and
sufficient purge throughput still need validation. Arithmetic widths and overflow behavior also
need an explicit definition in the consolidated documents. These are not resolved by condensation.

## Out of Scope

Validator reward distribution and cost coverage; token issuance and bridging; data availability
on other chains; RPC query serving; and history pricing. Calldata can outlive an entity in block
history, so entity expiry does not itself remove the cost of retaining that history.
