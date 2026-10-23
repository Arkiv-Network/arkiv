# Arkiv Tokenomics

Status: third draft for discussion, 2026-10-15. Covers Arkiv fees and where they go. Builds on
[GolemDB Metering](golemdb-metering.md), which prices compute and leaves time to the host.

## Contents

- **Context:** [Why](#why) · [Principles](#principles) · [Scope](#scope)
- **Fees:** [Inputs from GolemDB](#inputs-from-golemdb) · [Fee Formula](#fee-formula) · [Compute Cost](#compute-cost) · [Fee Schedule](#fee-schedule) · [Storage Units](#storage-units) · [Fees per Operation](#fees-per-operation) · [Limits](#limits) · [Purge Reserve](#purge-reserve)
- **Execution:** [Block Processing](#block-processing) · [Purge Budget](#purge-budget) · [Gas Accounting](#gas-accounting) · [Transaction Fee Lifecycle](#transaction-fee-lifecycle) · [Estimation](#estimation)
- **Flows:** [Token Flows](#token-flows) · [Storage Pool](#storage-pool)
- **Example:** [Worked Example](#worked-example-one-entitys-life)
- **Open points:** [Open Decisions](#open-decisions)

## Why

GolemDB metering prices the work a call causes: compute, and storage for the write and its bounded
history. It has no notion of time. Arkiv entities live for a chosen number of blocks, and full
nodes must hold their bytes for that whole lifetime. Arkiv fees add that missing dimension, turn
both into what a sender pays, and say who receives it.

## Principles

1. **Each cost is counted once and priced by its own scarcity.** Nodes bear three costs: work in
   each block, holding bytes over time, and removing those bytes later. These costs should be priced separately, so storage close to capacity must not make compute expensive and a busy block must not make storage expensive.
2. **Collect while the sender is present, at that moment's price; never refund.** After the
   transaction nobody is left to charge, and prices change. A refund at a later price than the
   purchase is arbitrage and should be discouraged: Ethereum's storage refunds made GasToken possible and were cut back by
   EIP-3529.
3. **Pay whoever bears the cost, while they bear it.** The proposer that includes a transaction
   is one of the nodes that store its bytes, not the only one. If the proposer received the
   storage fee, it could store its own data for free: the fee it pays would come straight back to
   it, while all other nodes carry the bytes.
4. **Sell only forward capacity the protocol can honour.** A lifetime is a forward contract:
   price changes reach new purchases, never the commitments already sold. Those must be bounded
   when they are sold.

## Scope

In scope: what an Arkiv transaction pays, how that amount is derived from GolemDB costs and
entity lifetime, how it is reserved and settled, and where it goes.

Token flows are designed for a BFT validator set with rotating proposers. Today's single
sequencer is the case where one operator holds every role.

Out of scope for now:

- The token itself: denomination, issuance, bridging, and rewards beyond fee distribution.
- Data availability: posting transaction data or commitments to Ethereum, long-term storage on
  Arweave or similar. A future cost that would be priced as its own resource (principle 1).
- RPC query serving: a node cost, not priced on chain.
- Whether fees cover the cost of running a node. In the single-sequencer phase, the project
  subsidises node operation; the check belongs to the staking and BFT work.
- The execution pipeline beyond what fees require: envelope validation, RPC block tags,
  restricting EVM features.

## Inputs from GolemDB

| Input | Source | Used for |
| --- | --- | --- |
| Compute cost per call, in GolemDB cost units | receipt ([metering D2, D6, D7](golemdb-metering.md#d2-write-cost-model)) | compute gas |
| Remaining budget per call | Budget ([metering D8](golemdb-metering.md#d8-budget-rollback-and-commit)) | stopping a transaction at its gas limit: the call returns `OutOfBudget{spent}` |
| Cell bytes, index bytes, cells, indexed cells per record | [`#meta`](golemdb-metering.md#the-meta-cell) | storage units, and from them the storage fee |
| Maximum deletion cost per record | [metering D5](golemdb-metering.md#d5-record-shape-and-deletion-bound) | purge reserve |
| `#minRetention` | [design §4 `#params`](golemdb-design.md#params-recordid-0) | not charged again (below) |

GolemDB charges failed and rolled-back calls for the work done (metering R6). Arkiv passes that
on: charged work stays charged when an entity operation fails. A failed transaction pays its
compute gas only. It bought no storage, so it pays no storage fee and no purge reserve.

**Dependency on read metering.** Arkiv puts two GolemDB reads into consensus gas: the owner point
`get` behind every patch, extend and delete, and the purge query. Both need their metering
specified ([metering review finding F1](golemdb-metering.md#f1-high-read-metering-lacks-a-complete-reference-execution))
before fees go live.

## Fee Formula

A transaction pays one fee with three parts, one for each cost of principle 1: work in the
including block, holding bytes over time, and removing them later. GolemDB prices the work;
Arkiv adds time and removal.

```
fee      = gas_used × effective_gas_price
gas_used = tx_gas + Σ over the batch's ops op_gas

tx_gas   = ceil(gas_fee_factor × (tx_intrinsic + tx_data + settlement))
op_gas   = compute_gas(op) + storage_gas(op) + purge_gas(op)

compute_gas(op) = ceil(gas_fee_factor × (arkiv_op_cost + golemdb_cost))
storage_gas(op) = ceil(storage_price × unit_blocks(op) / base_fee)
purge_gas(op)   = ceil(purge_price × purge_bound(op) / base_fee)
```

- `effective_gas_price`: `base_fee + ceil(tip × computeGas / gas_used)`, where `tip` is the
  priority fee per gas as in EIP-1559: `min(maxPriorityFeePerGas, maxFeePerGas − base_fee)`.
- `gas_fee_factor`: converts GolemDB cost units into gas. A calibration constant.
- `base_fee`: the EIP-1559 base fee, derived from compute gas only and never below
  `min_base_fee > 0`.

**Compute part.** `tx_gas` and every `compute_gas(op)`: the work done in the including block,
Arkiv's and GolemDB's, in GolemDB cost units converted into gas. Their sum is the receipt's
`computeGas` and counts against the block gas limit. Its terms are in
[Compute Cost](#compute-cost).

**Storage part.** Every `storage_gas(op)`: the storage the operation buys, for the blocks it
buys it. Their sum is the receipt's `storageGas`.

- `storage_price`: wei per storage unit per block. The capacity lever (below).
- `unit_blocks(op)`: storage units times blocks bought: `storage_units × lifetime` for a create,
  `storage_units × added_blocks` for an extend, `Δ storage_units × remaining_lifetime` for a
  growing patch, otherwise 0 ([Fees per Operation](#fees-per-operation)).
- `storage_units`: the record's bytes plus per-entry overhead ([Storage Units](#storage-units)).

**Purge part.** Every `purge_gas(op)`: the reserve for removing the entity later, when no sender
is present to pay. Their sum is the receipt's `purgeGas`.

- `purge_price`: wei per GolemDB cost unit of purge work. Fixed by the admin, not by congestion.
- `purge_bound(op)`: the D5 deletion bound for a create, its increase for a patch that raises
  it, otherwise 0; in GolemDB cost units ([Purge Reserve](#purge-reserve)).

What the sender pays per part:

- **Compute:** `base_fee × computeGas`. Follows congestion.
- **Storage:** `base_fee × storageGas`, which is `storage_price × unit_blocks` per operation,
  rounded up.
- **Purge:** `base_fee × purgeGas`, which is `purge_price × purge_bound` per operation, rounded
  up.
- Storage and purge are converted to gas by dividing by the base fee, and charged by multiplying
  by it again. The two cancel out: neither price moves with congestion.
- **Tip:** priority fee on compute gas only, folded into `effective_gas_price`. Arkiv charges
  exactly `gas_used × effective_gas_price`, so the standard fee formula stays exact; rounding up
  overcharges by less than `gas_used` wei. Storage and purge pay no tip, so the proposer earns
  nothing in proportion to storage bought (principle 3).
- **Ordering by tip is unchanged.** The proposer earns the signed priority fee per unit of compute
  gas, and block capacity is compute gas only, so the transaction pool and the builder can rank
  by the signed priority fee as usual. Only RPCs that derive the tip from
  `effectiveGasPrice − baseFee`, such as `eth_feeHistory` reward percentiles and gas price
  oracles, see it diluted over storage and purge gas; they must use the tip on compute gas.

**Why storage is converted into gas.** Wallets and libraries only know `gas × price`. Converted,
the storage fee travels in standard fields: `eth_estimateGas` returns the whole fee, and the
signed `gasLimit × maxFeePerGas` caps it. A separate cap is not needed, because the storage price
is set by an admin and announced ([Fee Schedule](#fee-schedule)), so the sender knows it before
signing. An EIP-4844-style split, with a storage dimension, its own `maxFee` field and a new
transaction type, becomes the better choice if storage is ever priced by demand.

**Why not one gas dimension, as in Ethereum.** Ethereum prices creating a storage slot once, for
ever, in the same gas as compute (20,000 gas for `SSTORE` zero to non-zero). State growth is then
bounded only through the block gas limit: every gas limit increase raises it too, and a time
dimension is hard to add later. Arkiv's lifetimes give that dimension from the start.

**Capacity lever.** `storage_price` makes Arkiv cheap while the chain has spare capacity and
expensive as committed entity data approaches it. It is set by an admin, as a weight, not by an
automatic curve: node disk capacity may grow during the chain's lifetime, which an automatic
mechanism would have to model. Because the price is in token while node costs are in fiat, the
admin also tracks the token price. The rule the admin follows is
[O3](#open-decisions).

**Retention is not charged again.** GolemDB's write-byte weights already prepay residency of
`#minRetention` commits of history ([metering D4](golemdb-metering.md#d4-storage-and-size-counting)).
`unit_blocks` therefore covers live residency only, from write to expiry. That history
residency is charged in the compute part, at the congestion price, which principle 1 would
otherwise rule out. It is accepted because `#minRetention` is short: it covers validating new
blocks and stable result sets for paged queries, so its residency is negligible next to an
entity's lifetime.

**Arithmetic.** Cost units and gas are checked `u64`; token amounts are checked `u256`, like EVM
balances. Conversions round up, per operation and part; the overcharge is at most one gas unit
each. An overflow fails the transaction, as in
[metering D8](golemdb-metering.md#d8-budget-rollback-and-commit).

## Compute Cost

The terms of the compute part, in GolemDB cost units:

- `tx_intrinsic`: fixed per transaction. Checks the envelope, recovers the sender address, and
  reads the sender's balance and nonces.
- `tx_data`: the transaction's encoded bytes × `tx_byte`, as Ethereum prices calldata. Covers
  propagating the bytes and keeping them in block history until pruning, both per-block
  resources, so it is part of the compute dimension and counts against the block gas limit. It
  does not cover the entity's state, which the storage part prices. This is the charge
  [metering D7](golemdb-metering.md#d7-immutable-log-metering) refers to when it lets Arkiv run
  the immutable log at zero weights.
- `arkiv_op_cost`: Arkiv's stateless work per entity operation: decoding it and checking it
  against Arkiv's rules (lifetime bounds, sizes, annotation rules). One weight per operation
  type plus a per-byte term, in the style of GolemDB's admission cost.
- `golemdb_cost`: every GolemDB call the operation makes, priced by GolemDB metering,
  including the reads that authorize it. An owner's `delete` is paid by the owner like any other
  call; the purge reserve pays only for the purge.
- `settlement`: fixed per transaction ([Transaction Fee Lifecycle](#transaction-fee-lifecycle)).

Every state access, Arkiv's included, goes through GolemDB and is metered there. Arkiv prices
only its own stateless work, with its own weights in GolemDB cost units: one meter, one unit,
one set of counting rules for state.

The entity operations of one transaction form an all-or-nothing batch. Each Arkiv operation is
made of GolemDB calls:

| Arkiv operation | GolemDB calls |
| --- | --- |
| create | `create`, with Arkiv's own cells (owner, `$expiresAt`, `$rate`) and the expiry index join used by the purge |
| patch | `get` of the owner cell, then `patch`, including `$rate` when the size changes |
| extend | `get` of the owner cell, then `patch` of `$expiresAt` and `$rate`: leave the old expiry term, join the new one |
| delete | `get` of the owner cell, then `delete` |

Arkiv's own cells count in `#meta` like any other cell, so they are part of `storage_units`.
Each operation also updates the [Storage Pool](#storage-pool) record.

## Fee Schedule

Arkiv's weights (`tx_intrinsic`, `tx_byte`, `arkiv_op_cost`, `settlement`, `gas_fee_factor`,
`storage_price`, `purge_price`, `cell_byte_units`, `index_byte_units`, `c_cell`, `c_idx`) and
limits (`max_lifetime`,
`max_block_storage_units`, `max_live_bytes`, `purge_budget`) live
in GolemDB's cost schedule as host-declared weights, named `host.*` ([metering D9](golemdb-metering.md#d9-cost-schedules)). GolemDB stores, validates
and activates them with its own weights and never prices with them.

Arkiv uses the cost schedule not only for prices but also for its protocol limits. The limits
decide block validity, so a change must activate at the same committed head on every node, can
be announced ahead, and must come from the same snapshot as the prices of the same block. The
schedule already provides exactly that, so Arkiv reuses it instead of keeping the limits in a
record of its own with a second lifecycle and admin path. GolemDB does not enforce Arkiv's
limits; Arkiv reads them from the snapshot and enforces them itself. `min_base_fee` is a chain
parameter; the estimation depth `k` is a node setting ([Estimation](#estimation)).

- **One snapshot.** Arkiv reads its weights from the pricing snapshot its GolemDB calls are
  priced with, so every fee in a block uses GolemDB and Arkiv weights from the same `priced_at`.
- **One admin path.** Weight changes, GolemDB's and Arkiv's, arrive in one permissioned Arkiv
  admin transaction, which Arkiv forwards to GolemDB's metering API. A GolemDB rescale and the
  matching `gas_fee_factor` change land in the same block.
- **Announced changes.** A weight change can be installed as a weights-only model version,
  active at least `#minActivationDelay` blocks later, so users see a new price before it applies.
- **Immediate patches, for now.** Until real-world use shows the weights are about right, Arkiv's
  admin transaction may also patch weights immediately. The sender stays protected:
  `gasLimit × maxFeePerGas` caps the fee, so a weight rise just before inclusion makes the
  transaction run out of gas and revert, paying for the compute done, never overcharges it. Once
  the weights have settled, changes go through the activation delay only.
- **Missing weights.** An operation that needs a missing Arkiv weight is unavailable, never free.
- **Visibility.** `arkiv_getFeeSchedule` returns the active schedule, GolemDB and Arkiv weights,
  and any pending one with its activation block.

## Storage Units

```
storage_units = cell_bytes × cell_byte_units + index_bytes × index_byte_units
              + cells × c_cell + indexed_cells × c_idx
```

All four counts come from `#meta`. `c_cell` and `c_idx` are per-entry overheads in
bytes-equivalent: keys, trie leaves, bitmap containers, so many small cells are priced
correctly. Cell bytes and index bytes have their own factor, both 1 at first: one more weight
costs little, and if measurements show that an index byte costs nodes more than a cell byte,
the ratio changes without a code change. Weights are integers, so a ratio that is not a whole
number is set by scaling all four weights together and lowering `storage_price` by the same
factor, in one model version.

## Fees per Operation

Proposed, compute cost aside:

| Operation | Storage fee | Purge reserve |
| --- | --- | --- |
| create | `storage_units × lifetime` | D5 bound |
| extend | `storage_units × added_blocks` | none |
| patch that grows the record | `Δ storage_units × remaining_lifetime` | top-up if the D5 bound grows |
| patch that shrinks or keeps size | 0; no refund for the shrink | top-up if the D5 bound grows |
| delete by the owner | none; no refund of the remaining lifetime | unused; stays in the pool, no refund |
| expiry purge | none: no sender | pays the purge |

Each storage fee is multiplied by `storage_price`. `Δ storage_units` is taken from `#meta` before
and after the patch, which the patch reads anyway. The storage fee follows the record's bytes;
the purge reserve follows its cell and indexed-cell counts, which set the D5 bound. The two can
move apart: a patch that adds cells but shrinks a payload pays no storage fee but a reserve
top-up. A patch whose bound falls gets nothing back (principle 2).

Lifetime is bought, not rented: shortening it returns nothing (principle 2). Senders who want
flexibility buy short lifetimes and extend.

**Delete by the owner.** The owner pays the deletion as ordinary compute, on top of the purge
reserve collected at create, which is not refunded (principle 2). A delete long before expiry
also forgoes the lifetime already paid. Both stay in the [Storage Pool](#storage-pool) as
surplus. Deleting before expiry is therefore the expensive path: applications should choose
lifetimes that let entities expire rather than delete them.

## Limits

Three limits bound what the protocol sells (principle 4). They live in the cost schedule with the
prices ([Fee Schedule](#fee-schedule)) and decide block validity.

- **Maximum lifetime.** A create or extend sets the expiry at most `max_lifetime` blocks ahead.
  An entity lives longer only by being extended again. This also bounds the fee and the
  arithmetic.
- **New storage per block.** A block accepts at most `max_block_storage_units` new storage units,
  summed over creates and growing patches. A transaction that would exceed it reverts and pays
  for the compute done, like one that breaks the live storage ceiling. A growing patch learns its
  units only during execution, so excluding the transaction instead would make that execution
  free work for the builder.
- **Live storage ceiling.** `live_bytes`, a cell of the `arkiv_state` record, counts the raw bytes,
  `cell_bytes + index_bytes` from `#meta`, of all entities not yet purged. It rises on creates and
  growing patches and falls on shrinking patches, deletes and purges. A create or growing patch
  fails if `live_bytes + new_bytes > max_live_bytes`; an extend adds no bytes, so it is not
  checked. Between sales, the committed storage only falls, so checking the current total at sale
  bounds every future block. The admin raises `max_live_bytes` as node disks grow. The count runs
  until the purge, because the disk holds the bytes until then.

  The ceiling counts raw bytes, not `storage_units`, because the byte counts in `#meta` depend on
  no weight: a create, a patch and the purge add or subtract exactly the same numbers, whatever
  weight changes happen in between. A count in `storage_units` would drift with every change of
  `c_cell`, `c_idx` or the byte factors. The per-block cap can stay in `storage_units`, since
  weights do not change within a block.

## Purge Reserve

Expiry deletes entities without a sender to charge. At create, Arkiv collects the entity's D5
maximum deletion cost as `purge_bound`; when a patch raises the bound, it collects the
difference. Both are priced at the fixed `purge_price`, not at the base fee: the purge runs later,
at a congestion nobody can know at collection, so the reserve depends neither on the base fee
at collection nor on the one at purge (principle 1). The reserve is paid into the
[Storage Pool](#storage-pool)'s balance; no reserve is kept per entity.

The purge spends it: the pool burns the purge's actual cost at `purge_price`, as the base fee of
other compute is burned ([Token Flows](#token-flows)). An entity deleted by its owner is never
purged; its reserve stays in the pool as surplus. Per
[metering D9](golemdb-metering.md#d9-cost-schedules), the bound is computed at the weights and
trie depth current when it is collected. The purge's actual cost may differ:

- A reserve above the actual cost leaves its excess in the pool.
- A reserve below it, after a weight rise or a deeper trie, is covered by the pool's surplus.

With a fixed price, the reserve depends only on the record's shape and the schedule; congestion
plays no part. The only remaining fluctuation is the token's real value.

## Block Processing

One block is one GolemDB branch from the parent block's commit: the purge first, then the user
transactions, then one end-of-block write and the commit. This section is the overview for
implementing it; the rules themselves are in the sections it links.

**Inputs at the start of the block.**

| Input | From |
| --- | --- |
| Pricing snapshot: GolemDB weights, Arkiv weights and limits | the branch base commit, one `priced_at` for the whole block ([Fee Schedule](#fee-schedule)) |
| `base_fee` | the parent block's compute-only `gasUsed`, by EIP-1559, at least `min_base_fee` |
| `arkiv_state` record: `balance`, `R`, `L`, `purge_cursor`, `live_bytes` | committed state |

**In-memory block state.** Kept by the executor for the whole block. Each transaction's changes
are buffered with it and discarded if it reverts.

| State | Limit |
| --- | --- |
| Compute gas used | block `gasLimit` |
| New storage units in this block | `max_block_storage_units` |
| `live_bytes` | `max_live_bytes` |
| Pool changes: `balance`, `R`, `L` | none; written once at the end |
| Purge work spent | `purge_budget` |

**Sequence.**

1. **System step: purge.** Not a transaction: no sender, no gas, no receipt. With `purge_budget`
   cost units, outside the block `gasLimit`, delete expired entities
   from `purge_cursor` in record-ID order until the budget or the backlog runs out
   ([Purge Budget](#purge-budget)). Per entity: burn its actual cost at the fixed `purge_price`
   from the pool's balance, lower `live_bytes` by its bytes, and, in the first version, lower `R`
   by its `$rate`. Purge work feeds neither the base fee nor any receipt.
2. **User transactions.** For each candidate, in the order the proposer picks:
   1. Pre-check: skip the transaction if its compute upper bound does not fit the compute gas
      left ([Gas Accounting](#gas-accounting)).
   2. Run it as in [Transaction Fee Lifecycle](#transaction-fee-lifecycle). Only compute gas
      counts against the block `gasLimit`; storage and purge gas take no block capacity. A
      create or growing patch also checks the per-block storage cap and the live storage
      ceiling ([Limits](#limits)).
   3. Entities with `$expiresAt` at or before this block that the purge has not reached yet are
      invisible and immutable to every transaction in it.
   4. Settle: burn `base_fee × computeGas`, pay the tip to the proposer, and add
      `base_fee × (storageGas + purgeGas)` to the pool's in-memory balance.
3. **End of block.** Write the `arkiv_state` record once, as host-funded block overhead.
   With full accounting, first apply the expiry ledger entry for this block and release `R` to the
   validators ([Storage Pool](#storage-pool)).
4. **Header and commit.** The header's `gasUsed` is the compute gas of the user transactions, which
   sets the next block's base fee. How header and receipts reconcile storage and purge gas is
   [O7](#open-decisions). Commit the branch.

**Two executors on one branch.** The purge executor runs the system step, the transaction executor
runs the user transactions; both work on the block's GolemDB branch, in that order, and call the
same GolemDB `delete`. Which path a delete takes is decided by which executor calls it, not by any
marker in the block:

- **Purge executor:** calls `delete` with `Limited(remaining purge_budget)`, stops at the first
  `OutOfBudget`, and pays from the pool at `purge_price`.
- **Transaction executor:** an owner's delete runs inside a transaction, with the transaction's
  remaining gas as its budget, and is paid by the sender as compute gas.

Sharing the branch gives both executors the same pricing snapshot, captured at the branch base
([metering D9](golemdb-metering.md#d9-cost-schedules)), and lets the transactions see the purged
state. A transaction's rollback returns to its own checkpoint, taken after the purge, so a
reverted transaction never undoes a purge. Every validator runs both executors itself: the
purge's selection follows from committed state, so validators derive the same deletions
independently, and the block carries nothing about them.

**Why the purge comes first.** Without a backlog, the entities expiring in this block are gone
before any transaction runs, so an operation on one fails because the record no longer exists.
The bytes the purge frees count for the creates of the same block, which matters near
`max_live_bytes`. The purge's position changes nothing else for users: its budget is fixed and outside the
user gas limit, and its selection follows from the parent block's committed state. Entities
the budget does not reach stay expired and invisible until a later block purges them.

**What a validator re-checks.** Validators re-execute the block on their own branch and reject it
if any of these differ:

- the purge: the same entities, in the same order, within `purge_budget`; the selection follows
  from consensus state, so nothing about it travels in the block;
- every transaction's gas split, fee and receipt;
- `Σ computeGas ≤ gasLimit`, the per-block storage cap, and the live storage ceiling at every sale;
- the `arkiv_state` record after the block, and the state root.

## Purge Budget

An entity expires at its `$expiresAt` block E: from E on it is invisible to queries and `get`,
and accepts no patch, extend or delete: an operation on it returns `NotFound`, as for an entity
that never existed. With full pool accounting, the pool also stops paying for it at
E ([Storage Pool](#storage-pool)); the first version keeps paying until the purge.
Deleting it from state is a separate step, the purge, which may run later.

**The purge runs under a fixed per-block budget outside the user gas limit.** Each block, before
the user transactions, the purge may spend up to `purge_budget` GolemDB cost units. It never uses
user capacity, never feeds the base fee update, and unused budget is not handed to users. Nodes
are sized for the user gas limit plus `purge_budget`.

Why outside the user gas limit, and why fixed:

- **The base fee must reflect user demand only.** EIP-1559 raises the base fee when user blocks
  are fuller than the target. If purge work used user capacity, an expiry wave would fill blocks
  and raise the price for users who caused none of it.
- **A predictable user gas limit.** Handing unused purge budget back to users would be safe, since
  user capacity never falls below the user gas limit, and would add throughput in quiet blocks.
  But users and estimation could no longer know how much fits into a block.
- **Predictable base fee updates.** EIP-1559 bounds the base fee's rise to 12.5% per block
  because a block can hold at most twice the target. Bonus capacity would break that bound, and
  with it the usual wallet headroom of `maxFeePerGas` at twice the base fee. **TODO (verify):**
  the default `maxFeePerGas` headroom of MetaMask, viem and ethers.
- **Nodes do not care where the work sits.** Computation is computation: a fixed budget inside
  the gas limit would load nodes the same. Outside keeps the user gas limit, the base fee target
  and `eth_feeHistory` meaning what users expect.

**Purge order: a cursor on the expiry index.** `purge_cursor`, a cell of the `arkiv_state` record,
holds the oldest expiry block not yet fully purged. The purge runs first in the block, so the
block's branch still equals the parent block's committed state, and the purge selects from that
state. Each block:

1. Query the parent's committed state for `$expiresAt = purge_cursor`, in record-ID order, with
   an empty projection, which returns each entity's key, and a limit. The query is charged
   against `purge_budget` like any read (metering D6).
2. Delete the entities found, in that order, each with the remaining budget as its GolemDB budget.
   If a delete returns `OutOfBudget`, the purge stops for this block; the work spent counts against
   the budget, and the entities not yet deleted are first in line next block.
3. If every entity of that expiry block is deleted and budget remains, advance the cursor to the
   next expiry block and repeat from step 1, up to the current block.

The purge stops at the first entity that does not fit rather than skipping it, so the order stays
fixed. `purge_budget` must therefore be at least the largest possible deletion cost, the D5 bound
of a record at the caps `#maxRecordCells` and `#maxRecordIndexedCells` and the maximum modeled
trie depth; otherwise such a record would block the purge for good.

**Each expiry block is read once per block.** Queries read committed state only, and the index
reflects a branch's deletes only once the branch is committed: its changes are applied at seal.
Within one block, a second query of the same expiry block would still return the entities just
deleted, so the purge reads each expiry block once and works through that list. The deletes become
visible in the index when the block commits, and the next block's query at the cursor returns the
entities still left: no position within a term is needed. Entities expiring in the current block
join the backlog by being the term the cursor reaches next; extends and owner deletes move or
remove an entity from its term in committed state and need no backlog bookkeeping. After a quiet
stretch, the cursor steps through empty terms, each a cheap read of an absent term counted
against the budget, so catching up takes a few blocks.

**No key collisions with the backlog.** An expired entity stays in GolemDB until its purge, so its
record key is still bound. A create with the same key would fail with `AlreadyExists` at a time
that depends on the backlog. This cannot happen: Arkiv guarantees that entity keys never repeat a
key that existed before, so no create ever targets a record waiting in the backlog.

**Deleting is consensus; choosing need not be, but is.** A purge deletes committed state:
records, index terms, `#meta`, and with them the state root. Every node must apply the same
deletions in the same block, or state roots diverge. Proofs of absence, `live_bytes` and the
pool's reserve accounting depend on it too.

Which entities to purge could be left to the proposer, as transaction selection is: a purge is
valid if every purged entity has `$expiresAt` at or before the block and the work stays within
`purge_budget`. Arkiv selects deterministically with the cursor anyway, because it is simplest:

- No purge list in the block: validators re-derive the selection from consensus state, the index
  and record-ID order.
- No liveness rule or incentive: a proposer who never purged would let the backlog grow until
  `max_live_bytes` blocks creates. With the cursor, the budget is always spent while the backlog
  is not empty.
- One code path for proposer and validators.

What stays local is disk reclamation after the retention window: pruning history beyond
`#minRetention`.

## Gas Accounting

Standard fields keep their meaning and cover the whole fee; new fields show the split. Block
capacity counts compute only (principle 1).

**Transaction:**

| Field | Meaning |
| --- | --- |
| `gasLimit` | caps compute, storage and purge gas |
| `maxFeePerGas`, `maxPriorityFeePerGas` | standard EIP-1559; `gasLimit × maxFeePerGas` caps the whole fee |

**Receipt:**

| Field | Meaning |
| --- | --- |
| `gasUsed` | `computeGas + storageGas + purgeGas` |
| `effectiveGasPrice` | base fee plus the tip on compute gas spread over `gasUsed`; `gasUsed × effectiveGasPrice` is the exact fee |
| `computeGas` | new: compute gas |
| `storageGas` | new: storage gas |
| `purgeGas` | new: purge reserve gas |
| `storageFee`, `purgeFee` | new: `storageGas × base_fee` and `purgeGas × base_fee`, in wei |

No fee field, as in Ethereum: the fee is `gasUsed × effectiveGasPrice`.

**Block:**

| Field | Meaning |
| --- | --- |
| `gasLimit` | compute gas only |
| `gasUsed` | compute gas only; drives the base fee update and `eth_feeHistory` |
| `baseFeePerGas` | standard, at least `min_base_fee` |

**Block building differs from Ethereum.** Ethereum checks each transaction's `gasLimit` against
the gas left in the block before running it. Here a transaction's `gasLimit` includes storage
and purge gas, which take no block capacity, so that check would reject transactions whose compute fits.
Arkiv adapts block validity and block building:

- **Validity:** `Σ computeGas ≤ gasLimit`, checked on actual use.
- **Pre-check before execution.** The storage and purge gas of a create follow from its payload
  alone, before execution. The builder skips a transaction whose compute upper bound does not
  fit the compute gas left in the block, as Ethereum does with `gasLimit`:

  ```
  compute_upper_bound = gasLimit − Σ over its creates (storage_gas + purge_gas)
  ```

  Extends and patches add storage or purge gas that depends on the record's `#meta`, which the
  builder only reads during execution. For them the bound is conservative: a sender who sets a
  high `gasLimit` for an extend or a growing patch may wait for a block with more room.
- **A transaction that passes always fits.** Its actual storage and purge gas are never less than
  what the pre-check subtracted, so its compute stays within the bound and the block has room
  for it. Executing a transaction is therefore never free work for the builder.
- **No per-transaction cap** on storage or purge gas like EIP-7825. Compute gas per transaction is capped
  by the block `gasLimit`.

Client libraries are expected to drop or pass through unknown receipt fields, and an Arkiv
chain definition with a receipt formatter can expose the new ones, as OP Stack chains do for
`l1Fee`. **TODO (verify):** library behaviour for unknown receipt fields, in particular viem as
used by the Arkiv SDK, and viem's OP Stack receipt formatter as the model to follow. The receipts' `gasUsed` no
longer sums to the header's `gasUsed`; how to reconcile them for indexers is
[O7](#open-decisions).

## Transaction Fee Lifecycle

Two kinds of failure:

- **Invalid for inclusion:** bad signature, wrong nonce, maximum fee not covered by the
  balance. The transaction is not included and pays nothing. The pool may queue future nonces;
  execution requires the exact current nonce.
- **Included and reverted:** an entity operation fails or runs out of gas. The transaction is
  included, its nonce is consumed and the compute done is charged. It bought no storage, so it
  pays no storage or purge gas.

Sequence, in the block's branch:

1. Validate envelope, nonce and funding against the branch state.
2. Reserve the maximum fee, `gasLimit × maxFeePerGas`; increment the transaction nonce.
   Checkpoint.
3. Execute the batch, cheapest and least state-dependent checks first:
   1. Decode and validate every operation statelessly, before any state access. A malformed
      later operation fails before earlier ones have written anything.
   2. Per operation, in order: authorization reads, then the GolemDB writes. Each GolemDB call
      gets the remaining budget, `gasLimit` minus the gas used so far, in cost units. An
      operation adds its storage and purge gas once its storage units and counts are known: from
      the payload for create and extend, from `#meta` after the write for patch.

   On success, checkpoint. On failure, roll back once to step 2's checkpoint and discard
   buffered logs; the compute done stays charged.
4. Settle: charge `gasUsed × effectiveGasPrice`, release the unused reservation, distribute per
   [Token Flows](#token-flows) and update the [Storage Pool](#storage-pool). Checkpoint.

Releasing an unused reservation is not a refund of GolemDB cost (metering R6): it returns what
was never charged. Settlement work has a bounded, fixed charge included in the transaction's
cost, so pricing settlement does not recurse. Settlement cannot fail as a user revert: a failure there
means the pool's balance cannot cover a payment, an implementation fault, and the block
candidate is rejected.

## Estimation

Estimation runs the same executor on a disposable branch and never commits. GolemDB planning
runs with `Unlimited` budget ([metering D8](golemdb-metering.md#shared-planner-and-arkiv-usage-contexts));
resource protection (rate limits, timeouts) is the node's.

`eth_estimateGas` returns the whole fee in gas, so wallets and app builders need nothing
Arkiv-specific. Storage and purge gas rise when the base fee falls, and many clients use the estimate as
is (**TODO (verify):** which buffer, if any, MetaMask, viem and ethers add), so the node adds the
headroom itself: it converts storage and purge at the lowest base fee reachable
within `k` blocks, which falls by at most 12.5% per block.

```
estimate_base_fee = max(min_base_fee, base_fee × 0.875^k)
```

This raises only the reservation. Settlement charges the gas actually used at inclusion. At the
floor no headroom is needed, and the headroom never exceeds `base_fee / min_base_fee`.

`k` is the number of blocks the estimate must survive between estimation and inclusion. It lies
outside consensus: it changes what an estimate returns, never validity or what is charged. Each
node sets it in its configuration and falls back to a default of 5 when none is given. The
trade-off: too small, and a transaction whose base fee fell runs out of gas and pays for the
compute done; too large, and the sender's balance must cover a larger reservation. A backend
that signs at once needs 2–3 blocks; a human confirming in a wallet may need 5–15.

`arkiv_estimateFee` takes the same transaction as `eth_estimateGas` and an optional `k` that
overrides the node's. It returns:

- `gasLimit`: total gas with headroom for `k` blocks, the value to sign.
- `computeGas`, `storageGas`, `purgeGas`: the split at the current base fee.
- `computeFee`, `storageFee`, `purgeFee`: the same in wei at the current base fee, without tip.
- `baseFee`, `k`: the base fee the amounts are computed at, and the headroom in `gasLimit`.

## Token Flows

| Part | Amount | Goes to |
| --- | --- | --- |
| Tip | `gasUsed × effectiveGasPrice − base_fee × gasUsed`, about priority fee × `computeGas` | block proposer |
| Compute | `base_fee × computeGas` | burned |
| Storage | `base_fee × storageGas` | [Storage Pool](#storage-pool) |
| Purge reserve | `base_fee × purgeGas` | [Storage Pool](#storage-pool), burned at the purge |

Under BFT, every validator executes every transaction, so compute is borne by all of them, not
by the proposer. The base fee is therefore burned, as in EIP-1559: a proposer cannot fill its own
blocks at no cost, and only the tip goes to it. Storage pays for bytes every validator holds over
many blocks, so it goes to the pool and is released as the cost is borne (principle 3).

## Storage Pool

The storage pool holds what senders prepaid for storage and purges until expiry. The amount can be released to validators/block producers on a block by block basis (principle 3). 
It is a protocol-held account, kept in the `arkiv_state` record, a GolemDB record metered like
any other. `arkiv_state` also holds Arkiv's other protocol counters, `purge_cursor` and
`live_bytes`; "the pool" means its `balance`, `R` and `L`.

**First version.** Full pool accounting is deferred. The first version keeps what cannot be added
later, plus what is cheap:

- **`$rate` per entity.** The price an entity's storage was bought at cannot be reconstructed
  afterwards, so every entity records it from the start. With `$rate` and `$expiresAt` on every
  entity, `R` and `L` can be recomputed exactly when full accounting is introduced.
- **The `arkiv_state` record** with `balance`, `R`, `L`, `purge_cursor` and `live_bytes`. Storage fees and purge reserves
  accrue to `balance`.
- **Written once per block.** The executor keeps pool changes in memory, buffered with each
  transaction and discarded on revert, and writes the `arkiv_state` record once at the end of the block,
  as host-funded block overhead like the commit overhead of
  [metering D8](golemdb-metering.md#host-funded-blockcommit-overhead). The per-operation
  bookkeeping is fixed in-memory work that folds into `arkiv_op_cost`. The `arkiv_state` record is
  therefore neither written by every operation nor a contention point between transactions.
- **No expiry ledger.** `R` falls only when an entity is deleted or purged, so it includes expired
  entities still waiting in the purge backlog, and `L` falls a little faster than with the
  ledger. The purge budget keeps that lag short.
- **No release and no invariant check.** With a single sequencer, a release would pay
  the operator from fees the project collects anyway. `balance`, `R` and `L` are kept for
  monitoring; nothing rejects a block because of them.

The rest of this section describes the full accounting.

**State.**

| Term | Meaning |
| --- | --- |
| `balance` | The pool's token balance |
| `$rate` | An Arkiv-reserved, non-indexed u64 field per entity: the wei per block its storage pays out while it is live, at the price it was bought for. |
| `R`, release rate | The sum of the rates of all live entities: what the pool pays out per block |
| `L`, storage liability | Storage paid for but not yet released: Σ over live entities of `$rate × blocks until expiry` |
| Expiry ledger | Per future block, the sum of the rates of the entities expiring there, in records of its own ([proposal](#proposal-expiry-ledger-records)) |
| Surplus | About `balance − L`: what the pool holds beyond its storage liability, including the purge reserves not yet spent |

Purge reserves go into `balance`, and purges burn their actual cost from it; no reserve is
tracked per entity or in total. The purge work still owed is needed only by an off-chain monitor
(below).

**Changes.** Each event updates the state as follows. "To surplus" means `balance` stays while
`L` falls.

| Event | `$rate` | `R`, expiry ledger | `L` | `balance` |
| --- | --- | --- | --- | --- |
| create | `storage_price × storage_units` | `R += $rate`; add `$rate` at the expiry block | `+= $rate × lifetime` | `balance +=` storage fee and reserve |
| extend | `($rate × remaining + added_fee) / new_remaining`, rounded down | `R` updated; the rate moves from the old to the new expiry block | `+= added_fee`; the rounding remainder goes to surplus | `balance += added_fee` |
| growing patch | as extend, over the unchanged remaining lifetime | `R` and the expiry block's entry updated | `+= added_fee` | `balance +=` added fee and any reserve top-up |
| shrinking patch | scaled by `new_units / old_units` | `R` and the expiry block's entry updated | falls by `(old − new $rate) × remaining`, to surplus | unchanged |
| delete by the owner | removed | `R −= $rate`; removed from the expiry block | falls by `$rate × remaining`, to surplus | unchanged; the owner pays the deletion as compute, and the entity's reserve stays as surplus |
| each block | – | `R −=` the expiry ledger entry for this block | `−= R` | `R` is paid out from `balance` |
| purge | – | – (already left `R` at expiry) | – | the purge's actual cost at `purge_price` is burned |

Rounding up when fees are converted to gas leaves small excesses in `balance`, also surplus.

**Why `$rate` is per entity: an example.** The pool must know the price each entity's storage was
bought at, and that price cannot be recomputed later.

1. Block 0, `storage_price` = 10 wei per unit per block. Entity A: 100 units for 1,000 blocks
   pays 1,000,000 wei. `$rate`(A) = 1,000 wei per block. `R` = 1,000, `L` = 1,000,000.
2. Block 100, the admin raises `storage_price` to 20. Entity B: 100 units for 1,000 blocks pays
   2,000,000 wei. `$rate`(B) = 2,000. `R` = 3,000, `L` = 900 × 1,000 + 2,000,000 = 2,900,000.
3. Block 500, A's owner deletes A. A has 500 blocks left.

| At block 500 | With `$rate` | Recomputed from today's price, `20 × 100` |
| --- | --- | --- |
| A's rate taken out of `R` | 1,000, so `R` = 2,000: exactly B's rate | 2,000, so `R` = 1,000: B's storers get half of what B paid |
| A's remaining value taken out of `L` | 500 × 1,000 = 500,000 | 500 × 2,000 = 1,000,000: `L` falls short by 500,000 |
| A's entry taken out of the expiry ledger at block 1,000 | 1,000 | 2,000, though only 1,000 was added: at block 1,000, `R` would drop by the wrong amount |

The same holds for an extend or a patch: both start from the entity's current rate. Without
per-entity state, the only alternative is to pay out `current storage_price × live units` each
block. After step 2 that pays A's storage at 20 instead of the 10 bought, 900 × 1,000 = 900,000
wei more than A paid over its remaining lifetime, and the pool runs a deficit. `$rate` costs one
field per entity, 22 bytes plus `c_cell` in `storage_units`, and makes the release exact.

**Expiry ends payment, not the purge.** The expiry ledger takes an entity's rate out of `R` at
its expiry block, whether or not the purge has run. The pool therefore never pays for an expired
entity. Expired entities are invisible and immutable ([Purge Budget](#purge-budget)), so nobody
gains from one that is purged later.

**Release.** Each block's `R` goes to whoever stores the bytes: the validators, in proportion to
their stake. Today that is the single operator, who receives all of it. Every validator holds the
full state, so stake is a fair proxy for storage borne; it is simple to implement and rewards
node operators for staking meaningful amounts. Full nodes that do not validate are not paid.

**Invariant, monitored off-chain.** The pool is solvent if `balance ≥ L +` the purge work still
owed. Releases follow bytes actually held at the price they were bought for, so storage never
pays out more than it collected. What can draw on the balance is purge work that became dearer
after collection, after a weight rise or a deeper trie. A monitor computes the purge work still
owed from the `#meta` counts of the entities not yet purged and the current weights; nothing on
chain depends on it, and a breach is an alert, not a rejected block.

**Surplus.** It covers those shortfalls. The project seeds the pool at genesis, so they are
covered from the first block. What happens to a surplus well above what is needed is
[O2](#open-decisions).

### Proposal: Expiry Ledger Records

Not part of the first version. The expiry ledger cannot live in the `arkiv_state` record: Arkiv's records
are user records to GolemDB, capped by `#maxRecordCells`, while the ledger needs up to
`max_lifetime` entries.

- **`arkiv_state` record** with a fixed set of cells: `balance`, `R`, `L`, `purge_cursor` and `live_bytes`.
- **Ledger records**, one per bucket of expiry blocks, keyed by a key derived from the bucket
  number, holding the sum of the rates expiring in that bucket. A record is created on first use
  and deleted once its bucket has passed.
- **Buckets** of `ledger_bucket` blocks, for example 1,800 (an hour at 2 s). The pool stops
  paying an entity at the start of its expiry bucket. Storers lose at most one bucket of payment
  per entity, which stays in the pool as surplus; the user's `$expiresAt` is unchanged. Buckets
  cut the number of ledger records by `ledger_bucket` and let writes from different operations
  coincide.
- **Accumulated per block**, as the `arkiv_state` record in the first version: each touched ledger record
  is written once at the end of the block, so many creates in the same bucket cost one write.

Ledger writes per operation, before accumulation:

| Operation | Ledger writes |
| --- | --- |
| create | 1: add `$rate` to the expiry bucket |
| extend | 2: remove from the old bucket, add to the new one |
| patch that changes size | 1: adjust in the expiry bucket |
| patch that keeps size | 0 |
| delete by the owner | 1: remove from the expiry bucket |
| each block at a bucket boundary | 1: read and delete the bucket that starts |

Summing the `$rate` of the entities in each block's `$expiresAt` term instead would avoid the
ledger, but costs work proportional to the number of expiries, exactly at the expiry block and
outside any budget.

## Worked Example: One Entity's Life

One 1 KiB entity is bought for 3 months, grown, extended across a price rise, then expires and is
purged. Its owner pays about $0.0020 in total, 92% of it for storage, and the pool's storage
liability is released exactly: every wei of storage fee goes to the storers. Values are
illustrative, not calibrated.

**Assumptions.**

| Parameter | Value |
| --- | --- |
| Token, block time | $0.12; 2 s, so 1,296,000 blocks per 30 days |
| `gas_fee_factor` | 10 gas per GolemDB cost unit |
| `tx_intrinsic`, `settlement`, `tx_byte`, `arkiv_op_cost` | 2,000; 500; 2 per byte; 1,000 per operation (cost units) |
| `cell_byte_units`, `index_byte_units`; `c_cell`, `c_idx` | 1 each; 64 each |
| `storage_price` | 650,000 wei per unit per block; raised to 1,000,000 by an announced change before block 2,592,000 |
| `purge_price` | 10 gwei per cost unit (`min_base_fee` × `gas_fee_factor`) |
| `min_base_fee`, `max_lifetime` | 1 gwei; 7,776,000 blocks (6 months) |
| GolemDB costs | create 14,000, patch 7,800, extend 5,200 cost units, at the weights of the [metering worked example](golemdb-metering.md#worked-example-create-patch-delete) |

**The entity.** Four cells, two of them indexed. Bytes follow
[metering D4](golemdb-metering.md#d4-storage-and-size-counting).

| Cell | Kind | Cell bytes | Index bytes |
| --- | --- | --- | --- |
| `$owner` | attribute, `bytes20` | 35 | 28 |
| `$expiresAt` | attribute, `u64` | 27 | 20 |
| `$rate` | field, `u64` | 22 | – |
| `payload` | field, 1,024 bytes | 1,040 | – |
| **Total** | 4 cells, 2 indexed | 1,124 | 48 |

`storage_units` = 1,124 + 48 + 4 × 64 + 2 × 64 = **1,556**. The D5 bound is 300 admission +
250 record base + 3,050 system cells + 4 × 1,050 + 2 × 1,350 = **10,500** cost units, so the
purge reserve is 10,500 × 10 gwei = 105,000,000,000,000 wei ($0.0000126).

**Timeline.**

| Block | Event | Base fee | Storage bought |
| --- | --- | --- | --- |
| 0 | create, lifetime 3,888,000 blocks: expires at 3,888,000 | 2 gwei, tip 0.1 | 1,556 units × 3,888,000 blocks at 650,000 wei |
| 1,296,000 | patch: payload grows by 1,024 bytes, counts unchanged | 5 gwei, tip 0.1 | 1,024 units × 2,592,000 remaining blocks at 650,000 wei; no reserve top-up |
| 2,592,000 | extend by 3,888,000 blocks: expires at 7,776,000 | 1 gwei, no tip | 2,580 units × 3,888,000 blocks at 1,000,000 wei |
| 7,776,000 | expiry: invisible, immutable | – | – |
| 7,776,000 | purge, within `purge_budget`; no backlog assumed | – | – |

The extend's new expiry is 5,184,000 blocks ahead, within `max_lifetime`.

**Fees.** All amounts in wei.

| Step | `computeGas` | `storageGas` | `purgeGas` | `gasUsed` | `effectiveGasPrice` | Fee | Burned | Tip | To pool |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| create | 199,000 | 1,966,162 | 52,500 | 2,217,662 | 2,008,973,415 | 4,455,224,001,455,730 ($0.000535) | 398,000,000,000,000 | 19,900,001,455,730 | 4,037,324,000,000,000 |
| patch | 137,000 | 345,048 | 0 | 482,048 | 5,028,420,407 | 2,423,940,000,353,536 ($0.000291) | 685,000,000,000,000 | 13,700,000,353,536 | 1,725,240,000,000,000 |
| extend | 91,000 | 10,031,040 | 0 | 10,122,040 | 1,000,000,000 | 10,122,040,000,000,000 ($0.001215) | 91,000,000,000,000 | 0 | 10,031,040,000,000,000 |

How the create's numbers come about:

- `tx_gas` = 10 × (2,000 + 1,200 bytes × 2 + 500) = 49,000; `compute_gas` = 10 × (1,000 +
  14,000) = 150,000; `computeGas` = 199,000.
- Storage fee = 650,000 × 1,556 × 3,888,000 = 3,932,323,200,000,000 wei; `storageGas` =
  ⌈that / 2 gwei⌉ = 1,966,162.
- Purge reserve = 10 gwei × 10,500 = 105,000,000,000,000 wei; `purgeGas` = that / 2 gwei =
  52,500.
- `effectiveGasPrice` = 2 gwei + ⌈0.1 gwei × 199,000 / 2,217,662⌉; the fee is exactly
  `gasUsed × effectiveGasPrice`.

**Pool, after each step.**

| After | `$rate` (wei per block) | `R` | `L` | `balance` | `live_bytes` |
| --- | --- | --- | --- | --- | --- |
| create | 1,011,400,000 | 1,011,400,000 | 3,932,323,200,000,000 | 4,037,324,000,000,000 | 1,172 |
| patch | 1,677,000,000 | 1,677,000,000 | 4,346,784,000,000,000 | 5,762,564,000,000,000 | 2,196 |
| extend | 2,354,250,000 | 2,354,250,000 | 12,204,432,000,000,000 | 15,793,604,000,000,000 | 2,196 |
| purge | – | 0 | 0 | 15,688,604,000,000,000 | 0 |

- **Patch.** `L` was 1,011,400,000 × 2,592,000 before the patch; the patch adds its fee, and the
  new `$rate` = 1,011,400,000 + 650,000 × 1,024.
- **Extend.** `$rate` = (1,677,000,000 × 1,296,000 remaining + 10,031,040,000,000,000) /
  5,184,000 = 2,354,250,000, with no remainder. It blends the remaining storage bought at
  650,000 with the added blocks bought at 1,000,000; neither price can be read off the current
  `storage_price` alone.
- **Purge.** It burns the actual cost, 10,500 × 10 gwei, equal to the reserve because weights and
  counts did not change. `L` and `R` reach zero at expiry, which here is also the purge block. In
  the first version, a purge delayed by a backlog would leave `R` in place until then, so `L`
  would fall below zero for those blocks: one reason `L` is only monitored there.
- The `balance` columns are those of the first version, with no release. With full accounting,
  `balance` falls by `R` each block instead.

**What the example shows.**

- **Storage is priced by time, compute by congestion.** The create at 2 gwei and the patch at
  5 gwei pay compute in proportion to the base fee; their storage costs the same whatever the base
  fee.
- **Storage gas swings with the base fee.** The extend at the 1 gwei floor needs 10.0M storage gas
  for $0.0012 of storage, the size [review](arkiv-tokenomics-review.md) finding F1 is about.
- **The release is exact.** With full accounting the pool pays 1,011,400,000 × 1,296,000 +
  1,677,000,000 × 1,296,000 + 2,354,250,000 × 5,184,000 = 15,688,598,400,000,000 wei over the
  entity's life: exactly the three storage fees. What remains, 5,600,000,000 wei, comes from
  rounding storage gas up and stays as surplus.
- **Deleting instead of expiring costs more.** Had the owner deleted the entity at block
  2,592,000, it would have paid the deletion as compute, and the 105,000,000,000,000 wei reserve
  plus 2,173,392,000,000,000 wei of unused storage would have stayed in the pool as surplus.

## Open Decisions

| ID | Question | Current view |
| --- | --- | --- |
| O2 | Surplus above liability plus a margin: burned, released to storers, or used to lower `storage_price`? | open; an automatic on-chain rule would need the purge work still owed on chain: three running totals (entities, Σ cells, Σ indexed cells), since the D5 bound is linear in the counts |
| O3 | Rule the admin follows for `storage_price`: target utilization of a reference node's disk, and how the token price is tracked | open; without a stated target the lever can be neither checked nor predicted |
| O6 | Value of `purge_budget`, and of `purge_price` | open; the budget bounds how fast an expiry wave is purged |
| O7 | Receipts' `gasUsed` sum differs from the header's compute-only `gasUsed`: how indexers reconcile them | open; e.g. header fields for storage and purge gas |
| O9 | Values of `gas_fee_factor`, `storage_price`, `cell_byte_units`, `index_byte_units`, `c_cell`, `c_idx`, `min_base_fee`, the genesis pool seed, and who may change them | open |
| O10 | Purge events: a block-level system log entry per purged entity, so indexers learn which entities were removed without re-executing the block | proposed; informational only, validators do not need it |
