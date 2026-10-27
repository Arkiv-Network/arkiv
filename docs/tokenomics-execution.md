# Arkiv Tokenomics: Execution

Status: draft, 2026-10-25. The mechanics behind [Arkiv Tokenomics](tokenomics-spec.md), for
implementers: block execution, purge and pool accounting. The spec defines fees, prices and
limits and takes precedence. The gas-only milestone has a separate scope in the
[iteration-1 plan](tokenomics-iteration-1-plan.md).

## Contents

- [Inputs from GolemDB](#inputs-from-golemdb) · [Entity Operations](#entity-operations)
- [Fee Schedule Rules](#fee-schedule-rules) · [Storage Units and Limits](#storage-units-and-limits)
- [Block Processing](#block-processing) · [Purge](#purge)
- [Transaction Lifecycle](#transaction-lifecycle)
- [Pool Accounting](#pool-accounting) · [Proposal: Expiry Ledger Records](#proposal-expiry-ledger-records)
- [Example: the Pool over One Entity's Life](#example-the-pool-over-one-entitys-life)

## Inputs from GolemDB

| Input | Source | Used for |
| --- | --- | --- |
| Cost per call, in GolemDB cost units, which on Arkiv are gas | receipt ([metering D2, D6, D7](golemdb-metering.md#d2-write-cost-model)) | `golemdb_cost` |
| Remaining budget per call | budget ([metering D8](golemdb-metering.md#d8-budget-rollback-and-commit)) | stopping a transaction at its gas limit: the call returns `OutOfBudget{spent}` |
| Cell bytes, index bytes, cells, indexed cells per record | [`#meta`](golemdb-metering.md#the-meta-cell) | `storage_units`, `live_bytes` |
| Maximum deletion cost per record | [metering D5](golemdb-metering.md#d5-record-shape-and-deletion-bound) | `purge_bound` |

Failed and rolled-back calls still charge work done (metering R6): a reverted transaction pays
compute, but no storage or purge fee. Authorization `get` calls and purge queries enter
consensus gas; deterministic read metering is a prerequisite
([metering review F1](golemdb-metering.md#f1-high-read-metering-lacks-a-complete-reference-execution),
spec O16).

## Entity Operations

One transaction is an atomic batch of these GolemDB calls:

| Arkiv operation | GolemDB calls |
| --- | --- |
| create | `create`, with Arkiv's own cells (owner, `$expiresAt`, `$rate`) and the expiry index join the purge uses |
| patch | `get` of the owner cell, then `patch`, including `$rate` when the size changes |
| extend | `get` of the owner cell, then `patch` of `$expiresAt` and `$rate`: leave the old expiry term, join the new one |
| delete | `get` of the owner cell, then `delete` |

Arkiv's cells count in `#meta` and `storage_units`. Owner deletes cost compute gas, not the
purge reserve.

## Fee Schedule Rules

Use one branch-base snapshot, with one `priced_at`, for all GolemDB and Arkiv weights and
limits in the block. GolemDB stores, validates and activates `host.*` entries
([metering D9](golemdb-metering.md#d9-cost-schedules)); Arkiv prices with them and enforces
its limits. Missing required weights make an operation unavailable.

Forward the permissioned admin transaction to GolemDB's metering API. The spec's
[Fee Schedule](tokenomics-spec.md#fee-schedule) defines announced model versions,
`#minActivationDelay` and temporary immediate patches. Their activation boundary must preserve
the single snapshot on every node; the immediate-patch boundary remains unresolved.

## Storage Units and Limits

`c_cell` and `c_idx` cover per-entry overhead in bytes-equivalent. The cell/index byte factors
start at 1. To express fractional ratios with integer weights, scale all four together and
lower `capacity_rate` by the same factor in one model version.

Maintain `arkiv_state.live_bytes` as raw `cell_bytes + index_bytes` from `#meta` for all
entities not yet purged, including expired ones. Apply each create, patch, delete and purge's
raw-byte delta; extends add no bytes and need no byte-limit check. Raw counts remain valid
across weight changes; the per-block sales cap instead uses weighted storage units.

Apply the spec's [Limits](tokenomics-spec.md#limits) after each included transaction. Precheck
creates from payload; growing patches may require speculative execution. If a transaction
only exceeds remaining block capacity, discard its effects, leave it in the transaction pool
and charge nothing. Exceeding `max_block_storage_units` on its own instead reverts and charges
compute.

## Block Processing

One block is one GolemDB branch from the parent's commit: the purge first, then the user
transactions, then one end-of-block write and the commit.

Load the pricing snapshot, `ARKIV_ADDRESS` balance and `arkiv_state` from committed state.
Derive `base_fee` from the parent's compute gas by EIP-1559, floored at `min_base_fee`.
Buffer these counters and pool changes in memory; reverted transactions discard their changes:

| State | Limit |
| --- | --- |
| Compute gas used | `block_gas_limit` |
| New storage units in this block | `max_block_storage_units` |
| `live_bytes` | `max_live_bytes` |
| Pool changes: balance, `R`, `L` | none; written once at the end |
| Purge work spent | `purge_budget` |

1. **Purge:** system execution, no sender or receipt. Freed bytes are available to this block's
  creates. Purge and owner deletes use the same GolemDB `delete`, but separate budgets and payers.
2. **Transactions:** proposer order, subject to gas and storage capacity. Skip a transaction
  whose `gas_limit` exceeds remaining gas; apply storage admission as above. Execute using
  [Transaction Lifecycle](#transaction-lifecycle). Expired entities remain invisible and
  immutable even if not purged. Transaction checkpoints follow the purge and never undo it.
3. **End of block:** with full accounting, apply the expiry ledger entry, then write
  `arkiv_state` once as [host-funded overhead](golemdb-metering.md#host-funded-blockcommit-overhead).
4. **Header and commit:** `gasUsed = Σ compute_gas = Σ receipt.gasUsed`; commit the branch.

Validators re-execute and check purge selection/order/budget, transaction fees/change/receipts,
compute and storage limits, `arkiv_state` and the state root. Any mismatch rejects the block.

## Purge

`purge_budget` comes from the block's schedule snapshot, fixed within the block and adjustable
through the admin path between blocks. It is separate from user gas, never feeds the base fee,
and unused budget is not lent to users. The spec defines
[sizing and adjustment](tokenomics-spec.md#expiry-and-purge). Calls use `Limited(remaining
purge_budget)`; selection reads and failed attempts consume budget as well as deletions.
The pool pays the actual deletion charge at the active `min_base_fee`, burned.

**The cursor: draft pagination assumptions.** Use `purge_page_size = 256` as a protocol
constant, identical on proposer and validators, not a node configuration. For each expiry term:

1. If `purge_cursor > block_number` or no budget remains, stop. Otherwise query the parent's
  committed state for `$expiresAt = purge_cursor`, with ascending record-ID order, empty
  projection (keys only), offset 0, limit 256 and `total_matched = false`. This internal query
  includes expired entities; it does not apply the user-facing expiry visibility filter.
2. Pass `Limited(remaining purge_budget)` to the query. Success must return a complete page:
  exactly the first `min(256, matching_count)` keys, never a budget-truncated page. Account for
  its metered cost. On `OutOfBudget{spent}`, count `spent`, ignore any partial results and end
  the purge without advancing the cursor or deleting from that page.
3. Delete the page's keys in order with the remaining budget. If no budget remains before a
  deletion, stop. On `OutOfBudget{spent}`, count `spent` and stop; that call must leave the
  entity unchanged. Keep earlier successful deletions and their accounting, and leave the
  cursor on this term. Do not skip a key or retry within the block.
4. If every key on a short page (fewer than 256) was deleted, advance the cursor by one, even
  if the budget is now exhausted. An empty successful page also advances it. Repeat step 1;
  after exhausting the current block's term, the cursor is `block_number + 1`. A full page
  ends the purge without advancing the cursor, even if all 256 deletions succeeded and budget
  remains. The next block must confirm whether that term has more keys.

**One query per term per block.** Re-reading the fixed parent commit would return already-deleted
keys. Resume next block at offset 0 against its new parent; no within-term cursor or total-count
scan. New expiry targets must be in the future, so transactions cannot repopulate exhausted
terms. Empty-term scans also consume budget.

**Metering prerequisites remain open (O16).** Require complete pages, deterministic read costs
and budget failures, and positive cost for empty queries. A page limit bounds keys, not query
work. Progress requires bounded selection cost plus the D5 bound at record caps and maximum
modeled trie depth; deletion cost alone is insufficient. Validate that bound and the budget,
and the throughput cap of 256 deletions per expiry term per block, even with spare budget.

Selection and deletion are consensus rules: derive identical deletions and state roots without
a purge list in the block. Entity keys never repeat. Disk reclamation and pruning history
beyond `#minRetention` remain local.

## Transaction Lifecycle

**Invalid transactions** (bad envelope, wrong nonce, insufficient funding) are not included
and pay nothing. The transaction pool may queue future nonces; execution requires the current
nonce. **Included reverts** (operation failure, out of gas, insufficient value) consume the
nonce and charge compute spent, but roll back entity writes, value and logs.

Sequence, on the block's branch:

1. Validate envelope, nonce and funding of `value + gas_limit × max_fee_per_gas`.
2. Reserve `gas_limit × max_fee_per_gas`; increment the nonce. Checkpoint.
3. Move `value` to `ARKIV_ADDRESS` and log `ValueReceived` if nonzero. Execute the batch, cheapest and least
   state-dependent checks first:
   1. Decode and validate every operation statelessly, before any state access, so a malformed
      later operation fails before earlier ones write anything.
   2. Per operation: authorization reads, then the GolemDB writes, each with `gas_limit` minus
      the gas used so far as its budget. The operation then adds its storage and purge fee to a
      running total, from the payload for create and extend and from `#meta` after the write for
      a patch; a total above `value` reverts with `InsufficientValue`.

   On success, checkpoint. On failure, roll back to step 2's checkpoint, which undoes the
   `value` transfer, and discard buffered logs; the compute gas spent stays charged.
4. Settle: charge `gas_used × effective_gas_price` and release the unused reservation; keep
  storage and purge fees on success only; return nonzero `change` with `ValueReturned`;
  update the pool's in-memory state. Checkpoint.

Settlement is covered by fixed transaction overhead, so pricing does not recurse. A settlement
failure is an implementation fault rejecting the block candidate, not a user revert.

## Pool Accounting

**State.**

| Term | Meaning |
| --- | --- |
| balance | balance of `ARKIV_ADDRESS` |
| `$rate` | per-entity reserved, non-indexed `u64`: prepaid wei/block at the purchase price |
| `R` | Σ live `$rate`, owed per block |
| `L` | Σ `$rate × blocks until expiry`, owed in future blocks |
| expiry ledger | Σ `$rate` expiring at each future block ([proposal](#proposal-expiry-ledger-records)) |
| surplus | approximately `balance − L`, including rounding, unused storage and unspent purge reserves |

**Full-accounting updates.** "To surplus" means the balance stays while `L` falls.
`storage_price` and unrounded `added_fee` are defined in the
[spec](tokenomics-spec.md#where-the-money-goes).

| Event | `$rate` | `R`, expiry ledger | `L` | balance |
| --- | --- | --- | --- | --- |
| create | `storage_price × storage_units`, rounded down | `R += $rate`; add at the expiry block | `+= $rate × blocks` | `+= storage_fee + purge_fee` |
| extend | `($rate × remaining + added_fee) / new_remaining`, rounded down | moves from the old to the new expiry block | `+= delta_L` | `+= storage_fee` |
| growing patch | as extend, over the unchanged remaining lifetime | updated | `+= delta_L` | `+= storage_fee + purge_fee` |
| shrinking patch | scaled by `new_units / old_units` | updated | falls by `(old − new $rate) × remaining`, to surplus | `+= purge_fee`, if the bound grew |
| delete by the owner | removed | `R −= $rate`; removed from its expiry block | falls by `$rate × remaining`, to surplus | unchanged; the reserve stays as surplus |
| each block | – | `R −=` this block's ledger entry | `−= R` | `R` released (validator incentives, outside the spec) |
| purge | – | – | – | `−=` actual deletion cost × `min_base_fee`, burned |

**Rounding and liability.** On extend and growth, compute the rounded `new_rate` first, then
replace the entity's old remaining liability in `L`:

```
old_liability = old_rate × remaining
new_liability = new_rate × new_remaining
delta_L       = new_liability − old_liability
L             = L − old_liability + new_liability
```

For growth, `new_remaining = remaining`. `added_fee − delta_L` is surplus, separate from
transaction-level rounding. `delta_L` can be negative: blending can release old rounding
liability. Example: rate 1, remaining 2, added fee 2, new remaining 3 gives
`new_rate = floor((1 × 2 + 2) / 3) = 1`. Liability becomes 3, not 4; surplus gains 1 wei.

**Historical price and expiry.** Store `$rate`; recomputing it at current prices would alter
past purchases and misstate liabilities on deletion. With the ledger, remove its rate from `R`
at expiry, independent of purge timing: expired storage earns nothing.

**First version.** Store `$rate` for later reconstruction of live `R` and `L`, and
`arkiv_state` with `R`, `L`, `purge_cursor`, `live_bytes`. Per-operation bookkeeping is
in-memory work covered by `arkiv_op_cost`; write once per block. No expiry ledger, payouts or
on-chain invariant check. `R` falls only on delete or purge, lagging expiry.

**Deferred accounting rule.** Decrementing `L` by that lagging `R` can produce negative
liability. Define stored totals and numeric behavior before implementation; monitoring does
not resolve this. Do not infer wrapping, saturation or signed encoding. The bucket-ledger
proposal also remains unresolved. [O17](tokenomics-spec.md#open-decisions) collects the options
for paying the delay between expiry and purge, some of which let `R` fall at purge with `L`
still covered.

**Solvency monitoring.** Check `balance ≥ L +` outstanding purge work, estimated from unpurged
entities' `#meta` at current weights. Storage releases are bounded by purchases; increases in
weights, trie depth or `min_base_fee` can make purge cost exceed reserves and consume surplus.
The genesis seed provides a buffer, not a guarantee. A breach raises an off-chain alert;
the consensus policy for an unaffordable purge charge remains open.

## Proposal: Expiry Ledger Records

Deferred proposal, not the first version. Up to `max_lifetime` entries cannot fit in
`arkiv_state` under `#maxRecordCells`. Instead, key records by expiry bucket; create on first
use, delete at the bucket boundary. Each holds summed expiring rates and is written once per
block after accumulating changes.

For example, `ledger_bucket = 1,800` blocks (one hour at 2 s). Payment stops at the bucket's
start, retaining at most one bucket's payment per entity as surplus and reducing record count
by that factor. Handling creates or modifications after their bucket has been consumed remains
undefined; this is not yet an executable accounting rule.

| Operation | Ledger writes, before accumulation |
| --- | --- |
| create | 1: add `$rate` to the expiry bucket |
| extend | 2: remove from the old bucket, add to the new one |
| patch that changes size | 1: adjust in the expiry bucket |
| patch that keeps size | 0 |
| delete by the owner | 1: remove from the expiry bucket |
| each block at a bucket boundary | 1: read and delete the bucket that starts |

Summing each block's `$expiresAt` term instead requires unbudgeted work proportional to expiries.

## Example: the Pool over One Entity's Life

Continue the spec's [Example](tokenomics-spec.md#example): create at block 0 for 3,888,000
blocks; grow by 1,024 bytes at 1,296,000; extend by 3,888,000 at 2,592,000 after `capacity_rate`
rises from 14 to 21; purge at 7,776,000. Wei amounts; balance shown without payouts.

| After | `$rate` | `R` | `L` | balance | `live_bytes` |
| --- | --- | --- | --- | --- | --- |
| create | 217,840,000 | 217,840,000 | 846,961,920,000,000 | 1,896,970,000,000,000 | 1,172 |
| patch | 361,200,000 | 361,200,000 | 936,230,400,000,000 | 2,268,560,000,000,000 | 2,196 |
| extend | 496,650,000 | 496,650,000 | 2,574,633,600,000,000 | 4,375,080,000,000,000 | 2,196 |
| purge | – | 0 | 0 | 3,325,080,000,000,000 | 0 |

- **Create.** `L = 140,000 × 1,556 × 3,888,000`. Balance adds the rounded storage fee and
  1,050,000,000,000,000 wei reserve.
- **Patch.** `L` was 217,840,000 × 2,592,000; add 140,000 × 1,024 × 2,592,000 =
  371,589,120,000,000. `$rate = 217,840,000 + 140,000 × 1,024`.
- **Extend.** `$rate` = (361,200,000 × 1,296,000 + 210,000 × 2,580 × 3,888,000) / 5,184,000,
  with no remainder: storage bought at 140,000 and at 210,000 wei per unit-block, blended.
- **Purge.** Burn 105,000 gas × 10 gwei, equal to the reserve with unchanged deletion weights,
  counts and `min_base_fee`. This table assumes no expiry backlog; delayed first-version purge
  exposes the unresolved negative-liability issue above.
- **Full-accounting release:** 3,325,069,440,000,000 wei, exactly the three unrounded storage
  purchases here. The remaining 10,560,000,000 wei is rounding surplus.
- **Early delete instead of extend:** at 2,592,000, leave the reserve and
  `361,200,000 × 1,296,000 = 468,115,200,000,000` wei of unused storage as surplus.
