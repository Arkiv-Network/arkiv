# Arkiv entity pruning — specification (v0.2, recommendations applied)

Standalone. Supersedes `01-pruning-spec.md` by folding in the review's
accepted recommendations (`02-critical-review.md`). Arkiv-layer; Golem-DB is
used only via `count`, `query`, `delete`, `fork`, `discard`.

## TL;DR

- Pruning is a **system step at the start of every block**, run identically
  by producer, validators and execution nodes. No signer, no fee payer, no discretion.
- There is **no queue structure**. The queue is the engine-sorted result of
  `expiresAtBlock ≤ H − W` on consensus state. `W` (retention / bi-temporality window). (Alternatively we could consider having a cached queue if mechanisms would be put in place to make sure there is no divergence compared to a query over live-state and it gives significant performance boost)
- **Budget:** if anything is due, pruning may consume up to
  `G_cap = 80 %` of block gas; otherwise 0. No estimator. User gas is
  `G_total − G_prune`, so users always keep ≥ 20 % − ε.
- **Loop:** take entities in queue order; `delete` each with
  `Budget = remaining`; the engine aborts the first that does not fit; stop.
  Everything metered in the step — `count`, batch queries, successful
  deletes, the aborted attempt — counts as `G_prune`. Prune operation should delete the entity from database portion that is part of the state.
- **Liveness:** creation caps guarantee any single prune costs
  `≤ C_max ≤ G_cap`, so at least one entity is pruned per block whenever
  any is due. No special cases.
- **Who pays:** the creator, at creation and on any patch that changes
  prune cost — e.g. through a simulated `delete` in an estimation fork, added to the
  storage price. Released to nobody; it is a fee, not an escrow.
- **Backpressure:** the storage price multiplier takes the gas-weighted
  backlog as an input; a hard ceiling refuses creation above `N_max`.
- **Engine requirements:** (a) an `OutOfBudget` abort of a write leaves the
  branch unchanged

---

## 1. Constants and symbols

| Symbol | Meaning | Proposed |
|---|---|---|
| `H` | Height of the block being executed | — |
| `W` | Retention window after expiry, in blocks | 0 |
| `G_total` | Block gas limit | — |
| `P` | Pruning cap, % of `G_total` | 80 |
| `G_cap` | `floor(G_total · P / 100)` | — |
| `B` | Keys-only batch size | 256 |
| `R` | Gas per `CostUnit`, rational `R_num / R_den` | — |
| `C_max` | Upper bound on one entity's prune cost, enforced by creation caps | must satisfy `C_max ≤ G_cap` |
| `N_max` | Gas-weighted backlog above which creation is refused | — |
| `ε` | Largest unit price in the active `CostSchedule` | — |

All arithmetic is integer; `saturating_sub` where underflow is possible.

## 2. Entity lifecycle

```text
create(expiresAtBlock = E)        E > H at creation
   │  extend allowed while E ≥ H  (re-prices storage; prune cost unchanged)
   │  patch allowed while E ≥ H   (re-prices prune component, §6)
   ▼
E < H         expired: hidden from user queries, still stored     (only if W > 0)
   ▼
E + W ≤ H     due: physically deleted by the pruning step, in order
```

Hidden means Arkiv appends `expiresAtBlock ≥ H` to every conjunction group
of every user query and rejects `get` on expired keys. With `W = 0` the
hidden phase is empty and the conjunct is still required (an entity due at
`H` is deleted before any transaction in `H` runs, so the predicate is
redundant but harmless; keep it for `W > 0` compatibility).

Nothing due can be extended (`E ≤ H − W < H`) and nothing extendable is due.

## 3. The queue

```text
filter:  [ expiresAtBlock <= H − W ]
sort:    expiresAtBlock ASC, CreationBlock ASC, PositionInBlock ASC        (engine tie-break) (or use AutoIncrementalIndex ASC as tie breaker if makes more sense)
```

Evaluated on the base branch at the start of block `H`. Identical on every
node by the engine's determinism guarantee. Length: `count(filter)`.

## 4. Pruning step

Runs first in every block, before any transaction, on the block's base
branch. Block-candidate forks are taken afterwards, so every candidate
inherits identical pruning. The receipt `(pruned, G_prune)` is part of the
block and covered by the state hash.

```text
fn prune_step(base, H) -> PruneReceipt {
    cutoff = H.saturating_sub(W)
    filter = [expiresAtBlock <= cutoff]

    n      = count(base, filter, budget = G_cap)         // metered
    spent  = n.cost; pruned = 0
    if n.value == 0 { return (pruned, to_gas(spent)) }

    budget = G_cap
    loop {
        batch = query(base, {filter, sort: expiresAtBlock ASC, limit: B,
                             projection: keys_only},
                      budget = budget − spent)
        spent += batch.cost                              // also on OutOfBudget
        if batch is OutOfBudget or batch.is_empty() { break }

        for key in batch {
            match delete(base, key, budget = budget − spent) {
                Ok(r)               => { spent += r.cost; pruned += 1 }
                Err(OutOfBudget{s}) => { spent += s; return (pruned, to_gas(spent)) }
                Err(_)              => consensus_fault()
            }
        }
    }
    return (pruned, to_gas(spent))
}

G_prune = receipt.gas;   G_user = G_total − G_prune
```

Normative rules:

1. **Order is the queue order.** No entity is skipped for a cheaper one.
2. **Abort, never revert.** A `delete` that would exceed `budget − spent`
   is aborted by the engine at an accrual boundary; the branch is unchanged
   (§7a). `spent` of the abort is `≤ remaining + ε`.
3. **Stop at first abort.** The next block resumes at the same position
   because deleted records have left the index.
4. **Everything counts.** `G_prune` includes `count`, every batch `query`,
   every successful `delete`, and the aborted attempt.
5. **Bounds.** `G_prune ≤ G_cap + ε`; `G_user ≥ G_total − G_cap − ε`.
   If a hard `(100 − P) %` floor is required, use `budget = G_cap − ε`.

## 5. Liveness

- With `C_max ≤ G_cap` and `budget = G_cap` when anything is due, the head
  of the queue always fits. **At least one entity is pruned per block
  whenever any is due.**
- Per block: between `floor(G_cap / C_max)` and `floor(G_cap / C_min)`
  entities.
- `C_max ≤ G_cap` is checked at startup and on every `set_cost_schedule`;
  a schedule that violates it is rejected by Arkiv before installation.

## 6. Fees and backpressure

**Prune cost is prepaid by the creator.** At `create`, and at every `patch`
that changes the record's cells, Arkiv computes the prune component by
simulation in the estimation fork:

```text
f = fork(base); apply the create/patch on f; d = delete(f, key); discard(f)
prune_component = to_gas(d.cost) · margin      (margin ≥ 1, fee policy)
```

The charge is added to the storage price. It is a fee, not an escrow: no
party is paid at prune time, because pruning has no discretion to reward.

**Backpressure.** Let `L(H)` be the gas-weighted backlog: the sum of
prepaid prune components of all entities due at `H` (maintained in state as
an integer; incremented on creation/patch when the entity's `prune_at` is
known, decremented on prune). Then:

- Storage price multiplier `m(L)` is a non-decreasing integer function of
  `L`, protocol-defined (proposed: `1 + L / G_cap`, i.e. +100 % per block of
  backlog).
- Creation and extension are refused (`InvalidArgument` at the Arkiv layer)
  while `L(H) > N_max`.

Both inputs are consensus state at block start, so both are deterministic.

## 7. Golem-DB interface requirements

| # | Requirement | v0.1 status | Change class |
|---|---|---|---|
| a | An `OutOfBudget` abort of `create` / `patch` / `delete` leaves the branch byte-identical to before the call | Unspecified (only reads are covered) | **Breaking** to make normative; without it, every budgeted write in the system must be wrapped in `fork/commit/discard` and the cost model changes |
| b | `projection: Some([])` returns keys and versions only | Undefined | Additive |
| c | Range `count`, deterministic sort with key tie-break | Present | — |

No blockchain vocabulary crosses the boundary.

## 8. Determinism checklist

Inputs: `H, W, G_total, P, B, R, N_max`, active schedule version, base
branch, `L(H)`. No node-local state. Integer only. Reorgs revert the step
with the block. Same inputs ⇒ same `PruneReceipt` and branch hash on every
node.

## 9. Falsifying tests

1. Two nodes, identical base and `H` ⇒ identical receipt and branch hash.
2. Random queues crossing the cap mid-batch ⇒ `G_prune ≤ G_cap + ε`.
3. `k` entities each costing `C_max` ⇒ exactly `floor(G_cap / C_max)` per
   block; queue empty after `ceil(k / floor(G_cap / C_max))` blocks.
4. Entity with `E = H − W + 1` survives block `H`, is deleted in `H + 1`.
5. Extension at `E = H − 1` rejected; at `E = H` accepted and the entity is
   absent from `due(H + W)`.
6. Aborted `delete`: branch hash unchanged; `spent ≤ remaining + ε`.
7. Mass expiry of 10⁶ entities ⇒ every block admits `≥ G_total − G_cap − ε`
   user gas; `L(H)` decreases monotonically until zero.
8. Patch that adds attributes ⇒ prune component re-charged by the delta;
   `L(H)` updated accordingly.
9. `set_cost_schedule` making `C_max > G_cap` ⇒ rejected.
