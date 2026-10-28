# Purging: deleting expired entities

How expired entities are deleted from state, how far the code is from the target in
`docs/tokenomics-spec.md`, and the steps to get there.

- [Terms](#terms)
- [Target](#target)
- [Where `#meta` comes in](#where-meta-comes-in)
- [Today](#today)
- [Gaps](#gaps)
- [Steps](#steps)
- [Open decisions](#open-decisions)

## Terms

- **Purge:** deleting expired entities from state. The tokenomics documents use this word.
  It is a consensus rule: every node must delete the same entities in the same block.
- **History pruning:** GolemDB dropping old versions beyond `#minRetention`. Local to each
  node, not part of consensus, and already paid for by GolemDB's write weights. Not covered
  here.

## Target

From `docs/tokenomics-spec.md` ("Purge", "Expiry and Purge", "Limits") and
`docs/tokenomics-execution.md` ("Purge", "Pool Accounting").

1. **Expiry is immediate, deletion is not.** At `$expiresAt` an entity disappears from reads
   and accepts no operation. Deletion can happen later; until then its bytes count in
   `live_bytes`.
2. **A system step, not a transaction.** The purge runs before the block's transactions. No
   list of keys travels in the block: every node selects the same entities from the parent
   commit's `$expiresAt` index.
   - `purge_cursor`, kept in an `arkiv_state` record, is the oldest expiry block not yet
     cleared.
   - One query per expiry block per block, ascending record ID, a protocol-fixed page of 256
     keys.
   - A short page that is fully deleted advances the cursor; a full page or a budget failure
     leaves it in place for the next block.
3. **Its own gas budget.** `purge_budget` (6,000,000 gas to start) lives in the fee schedule.
   It sits outside the block gas limit and never feeds the base fee. Selection reads and
   failed attempts count against it as well as deletions. It must at least cover the largest
   possible deletion plus its selection read, or one large record stops the purge for good.
4. **Prepaid by the creator.**
   - A create pays `purge_gas`: the D5 deletion bound of its record. A patch that raises the
     bound pays the increase. Extends, transfers and owner deletes pay none.
   - The payment goes to the pool at `ARKIV_ADDRESS` as a purge reserve.
   - When the purge runs, the pool burns the actual deletion cost. The difference between
     reserve and cost goes to, or comes from, the pool's surplus.
   - An owner's early delete pays compute gas and leaves the reserve in the pool.
5. **Accounting between expiry and purge (O17).** An entity's storage payment ends at
   `$expiresAt`, but nodes hold its bytes until it is purged. Who pays for that delay is open.

## Where `#meta` comes in

`#meta` is GolemDB's per-record cell holding four counters over the record's user cells:
cells, cell bytes, indexed cells, index bytes. GolemDB keeps it up to date with every write.
Its metering document (`golemdb-metering.md`, "The `#meta` Cell" and D5) defines what the
counters are for. For the purge they decide four things.

1. **What a deletion costs.** By D5 the cost of deleting a record depends only on its cell
   and indexed-cell counts:

   ```
   delete cost = fixed part (admission, key read, record and system cells)
               + cells         × (read.cell + write.cell.delete + cell_trie_depth  × write.cell_trie_update)
               + indexed cells × (read.term + write.index.leave + index_trie_depth × write.index_trie_update)
   ```

   GolemDB stores the counts, not the cost, so the cost is always computed at the current
   weights and trie depth. It is exact for the current state and may fall later, for
   example after the database shrinks.
2. **What a create or patch prepays.** `purge_gas` is that cost at the time of the write: for
   a create, from the new record's counts; for a patch, the increase from the counts before
   to the counts after. The counts before come with the read every patch already makes
   (`get_entity_with_meta`); the changes come with the write's receipt (`WriteDetails`:
   cells created and deleted, index joins and leaves). This is the same pattern storage gas
   uses. Because the prepayment is priced at the weights of the day and the burn at the
   weights of the purge, the two differ by design: the difference is surplus or deficit.
3. **Whether a deletion fits the budget.** The purge can price a record from its counts
   before deleting it, instead of trying the delete and failing on budget. Reading `#meta`
   is a metered read, so this trades a read for a failed attempt.
4. **`live_bytes`.** It is the sum of raw `cell bytes + index bytes` over all records not yet
   purged; the purge subtracts the purged record's two byte counters. Raw bytes, not
   weighted storage units, so recalibrating the weights does not change the count.

Two bounds do not come from `#meta`:

- The minimum `purge_budget`, the cost of deleting the largest possible record, comes from
  GolemDB's caps (`#maxRecordCells`, `#maxRecordIndexedCells`): about 930,000 gas at the dev
  genesis caps.
- The outstanding purge work, for solvency monitoring, is D5 summed over every unpurged
  record's counts at current weights.

## Today

1. **Selection is node-local.** The payload builder keeps an in-memory map of entities by
   expiry, rebuilt from each block's logs
   (`crates/arkiv-reth-payload-builder/src/chain_pruning_map.rs`). For each new block it picks
   up to `MAX_PURGE_KEYS` (10) expired keys within `PURGE_GAS_LIMIT` (1,000,000) of modeled
   gas.
2. **The purge is a transaction.** The builder signs a `purgeExpired(keys)` transaction as
   `PURGE_CALLER` and puts it first in the block (`crates/arkiv-reth-payload-builder/src/lib.rs`).
   The executor accepts it only in its exact protocol form (`is_protocol_purge`), and
   `arkiv_purge_expired` deletes each listed key that has expired, skipping the rest
   (`crates/arkiv-reth-executor/src/lib.rs`).
3. **Gas is modeled, not metered.** `purge_cost` = 10,000 + 5,000 per indexed attribute
   (`crates/arkiv-interfaces/src/gas.rs`), from the decoded entity, not from `#meta`. A
   purge whose modeled gas exceeds the limit halts out of gas. No GolemDB budget is
   involved.
4. **Nobody pays.** The purge transaction debits no account and the pool burns nothing.
   `purge_gas` is 0 in tokenomics iteration 1, so no reserve is collected either.
5. **Validators check expiry, not selection.** Re-executing the transaction confirms each
   listed key had expired, but not that these were the keys to purge. With one sequencer this
   is acceptable.

Expiry itself is mostly in place: reads filter expired entities, and operations on them
revert. They revert with `Expired`, though, where the target answers `NotFound`, as if the
entity never existed.

## Gaps

| Target | Today | Effect |
| --- | --- | --- |
| Purge prepaid, pool burns the cost | not collected, not paid | deletion is free |
| Outside the block gas limit and the base fee | the purge transaction's gas counts in the block and feeds the base fee | purges compete with user transactions and move the base fee |
| ~55 purges per block for 1 KiB entities (spec sizing at 20% of the gas limit) | at most 10 per block | a backlog builds under steady create load |
| Selection from committed state, the same on every node | an in-memory map on the proposer, keys listed in the block | validators cannot check the selection; the map is rebuilt from logs |
| Budget metered by GolemDB, reads included | modeled gas for deletions only | the budget does not reflect real work |
| `live_bytes` falls at purge, bounded by `max_live_bytes` | not tracked | no storage capacity limit |
| An op on an expired entity answers `NotFound` | it reverts with `Expired` | a client can tell an expired entity from one that never existed |

## Steps

Each step stands on its own.

### 1. Collect the reserve and pay from it

Fits tokenomics iteration 1: one fee, all gas.

- Per op, compute `purge_gas` from `#meta` counts in D5's shape: for a create, from the new
  record's counts; for a patch, the increase from the counts before (the ownership read's
  `#meta`) to those after (the read's counts plus the write's `WriteDetails`); 0 otherwise.
  Until GolemDB prices deletes, use placeholder weights: a fixed part plus one weight per
  cell and one per indexed cell, in the fee schedule. `GasBreakdown` already has the field.
- Settlement credits the pool `base_fee × (storage_gas + purge_gas)` instead of
  `base_fee × storage_gas` (`credit_pool`).
- `arkiv_purge_expired` prices each deletion the same way, from the record's `#meta`, debits
  the pool by the total at the block's base fee, and burns it: no account is credited. The
  payload builder's pruning map prices with the same function, so selection and execution
  agree.
- If the pool cannot cover a purge, purge anyway and log the shortfall (see open decisions).

*Done when:* tests follow the pool's balance through create, expiry and purge, and money is
conserved with the burn included: sender payment = burned + tip + pool credit + value, and
pool balance = credits − purge burns.

### 2. Purge as a system step

- Run the purge in the block executor's pre-execution step, where reth already applies its
  system calls, instead of as a transaction. Today the stock `EthBlockExecutor` handles that
  step, so this needs a wrapper.
- Select from the parent commit's `$expiresAt` index with `purge_cursor`, as in the target.
  Keep `purge_cursor` and `live_bytes` in an `arkiv_state` record.
- Add `purge_budget` to the fee schedule; charge purge work against it and not against the
  block gas limit.
- Remove the in-memory pruning map, the `purgeExpired` protocol transaction and
  `is_protocol_purge`.

*Prerequisite:* GolemDB read metering and budgets (O16), so that selection work is charged
deterministically and a query fails on budget instead of returning a partial page.

*Done when:* validators recompute the selection; the purge is absent from `gasUsed` and the
base fee; a backlog drains at the budget's rate; one maximum-size record cannot stop the
purge.

### 3. Liability accounting

`$rate`, `R` and `L` per entity and for the pool, the expiry ledger, and a decision on O17.
This is the postponed 6d of tokenomics iteration 1, together with the surplus policy (O2).
It only matters once the pool pays out.

## Open decisions

| Question | Proposal | Spec reference |
| --- | --- | --- |
| Price of the burn at purge in step 1: the purge block's base fee, or the base fee the reserve was collected at? | the purge block's base fee; the difference is surplus or deficit, as the spec has it for `min_base_fee` | "Purge" |
| A purge the pool cannot cover | purge anyway, cap the burn at the balance, log the shortfall: a stuck purge is worse than a short pool | review follow-ups, "insufficient-funds policy" |
| Sizing and adjusting `purge_budget` | start at 6,000,000 gas; the admin raises it when the cursor falls behind | O6 |
| A log per purged entity, so indexers learn about removals | add it with step 2 | O10 |
| Read metering for selection | prerequisite for step 2 | O16 |
| Storage between expiry and purge | decide with step 3 | O17 |
