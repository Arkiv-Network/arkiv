# Proposal: exact expiry with a per-block cap and spill

Status: proposal, 2026-11-17. Not implemented in Arkiv. A reference
implementation exists in
[arkiv-state-db-experiment](https://github.com/scx1332/arkiv-state-db-experiment)
(issue #9), where the measurements below come from.

It is an alternative to the planned consensus purge (issue #292,
`docs/purging-spec.md` on `matthiaszimmermann/feat/tokenomics-init`); see
[Relation to the planned purge](#relation-to-the-planned-purge).

## Summary

Delete every entity exactly at its recorded expiry block, bound the deletions
of one block by a chain parameter, and when a write aims at a block that is
already full, move its expiry to the next block with room instead of
rejecting it. Deletion work per block stays bounded, no expired entity is
ever left in state, and writes do not fail under ordinary load.

## The problem

Removing expired entities costs work, and a block can only do so much of it.
There are two ways to bound that work, and each has a cost.

**Arkiv today: logical expiry, lazy purge.** An entity is dead once the block
number reaches `expiresAt`; the producer later removes at most 10 per block
(`MAX_PURGE_KEYS`) with the `purgeExpired` protocol transaction
([tokenomics v1, §2.10](../tokenomics_v1.md#210-expired-entities-and-the-purge)).

- The purge rate is 10 per block, 40 per second at 250 ms blocks. A small
  benchmark created 499 entities per second on `arkiv-reth-dev`
  ([results](https://github.com/scx1332/arkiv-state-db-experiment/blob/main/benchmarks/2026-11-17-sdk-side-by-side.md)).
  At that rate the backlog grows by about 460 dead entities per second once
  they start expiring, and it only shrinks when writes stop.
- Dead entities stay in the state and its indexes, so every read, count and
  proof has to exclude them, and the state root commits to rows that no
  longer exist for applications.
- Which entities a block removes is outside consensus. The producer chooses
  the keys from a local side database (`arkiv-pruning.db`) and lists them in
  the transaction; the other nodes check only that each listed key has
  expired (`arkiv_purge_expired`), not that these were the keys to remove. A
  block that purges nothing is valid, and the producer builds one whenever
  its database lags or the selection times out. Execution is still
  deterministic, because the keys are in the block: nodes do not diverge.
  But the purge is not a function of the state, and no rule makes a producer
  do it.
- The purge is unfunded (`purge_gas` is 0).

**Exact expiry with a hard cap.** The alternative, which the reference
implementation used first: the sweep at the start of block `h` deletes every
entity with `expiresAt == h`. To bound it, at most `expiry_per_block` (16)
entities may share an expiry block; a write that would exceed it fails with
`ExpiryCapacityFull`. Applications give every entity the same lifetime, so all
creates of one block share an expiry block: of 8 concurrent 16-create
transactions, 7 reverted.

## The rule

Parameters:

- `C` = `expiry_per_block`: the most entities that may expire at one block.
  A genesis parameter, 1 to 128; proposed default 128, the most operations a
  block can hold.
- `W`: the spill window, in blocks; proposed 64. It belongs in the genesis
  beside `C`. (The reference implementation has it as a protocol constant
  only because its chain parameters' encoding is fixed.)

For an operation that sets an entity's expiry (create, extend) and resolves,
by the existing rules, to block `E`:

1. Let `E'` be the smallest block in `E ..= E + W` that fewer than `C`
   entities expire at. Record `expiresAt = E'`.
2. If every block in the window is full, the operation fails with
   `ExpiryCapacityFull`.
3. A permanent entity has no expiry block and never spills, and no finite
   expiry spills into the permanent sentinel.
4. An extension to the block the entity is already recorded at changes
   nothing; an extension onto a full block spills like a create and frees the
   old block. "May not shorten" is checked on `E`, and `E' >= E`.
5. Genesis prefill rows do not spill: a row over capacity is an error.

The sweep: block `h` deletes exactly the entities recorded at `h`, at most
`C` of them, before any user transaction.

Rent is charged for the requested block `E`; the blocks up to `E'` are free.
A later extension is charged from `E'`, and deletes refund nothing, so the
free blocks are never paid twice and never paid out. The events
(`EntityCreated`, `ExpiryExtended`) and every read report `E'`.

## Properties

- **Bounded work.** A block deletes at most `C` entities; the block budget
  reserves `C × max deletion cost`.
- **No dead state.** An entity is in the state if and only if it is live, so
  reads, counts and proofs need no liveness filter and the state root commits
  only to live entities. Counts can be answered from index sizes.
- **Deterministic.** `E'` is a function of the state (the per-block expiry
  counts), so every node resolves the same block.
- **No hidden backlog.** Where the lazy purge lets dead entities pile up, this
  rule pushes expiry later, visibly, by at most `W` blocks, and then refuses
  writes. With `C` at least the operations a block can hold, creates that
  share a lifetime never spill at all: one block cannot add more than `C`.
- **Lifetime is a floor.** An entity lives at least as long as it paid for,
  never less.

## What it costs

- `expiresAt` can be up to `W` blocks later than requested (16 seconds at
  250 ms blocks). A client must read it back rather than assume it.
- An application that needs an entity gone by an exact block cannot rely on
  it when blocks are full. A per-operation "fail instead of spill" flag would
  cover that.
- Spilled blocks are stored without rent.
- Each probed block is a state read, so a spilling write costs up to `W + 1`
  extra reads. The reference implementation bounds them but does not charge
  them yet.
- The expiry schedule (entities per block) becomes consensus state.

## Relation to the planned purge

Issue #292 ("Move to consensus based purging") and `docs/purging-spec.md`
already plan to replace today's purge. Their target keeps expiry and deletion
apart: an entity disappears from reads at `$expiresAt`, and a system step
deletes it later, selecting from committed state with a `purge_cursor`, under
a `purge_budget` (6,000,000 gas to start) outside the block gas limit, prepaid
by a purge reserve collected at create.

That target and this proposal agree on most of it: the purge is a system step
before the transactions, every node derives the same deletions from committed
state, and its work is bounded per block.
Both remove the `purgeExpired` transaction and the pruning database, and both
find entities prefilled at block 0, which never appear in a log (#333, #334).
The prepaid purge reserve fits either.

They differ in what happens when more entities expire than a block can
delete:

| | Planned purge (`purge_cursor`) | This proposal |
| --- | --- | --- |
| Overload shows up as | a backlog of expired entities still in state | a later `expiresAt`, by at most `W` blocks, then a failed write |
| Who is affected | nobody directly; nodes hold dead bytes | the writer who aimed at the full block |
| Expired entities in state | yes, until the cursor reaches them | never |
| Reads, counts, proofs | filter by `$expiresAt` | no filter |
| Storage between expiry and purge | open (O17 in the spec) | does not arise |
| `live_bytes` | includes the backlog, so it can block creates | live entities only |
| Bound on the work | gas budget, metered by GolemDB | count `C`, sized for the worst case |
| Extra consensus state | `purge_cursor` | entities per expiry block |
| Recorded `expiresAt` | always as requested | may be later than requested |

The spec sizes the budget at about 55 purges per block for 1 KiB entities.
The benchmark above created about 125 entities per block, so under that
load the planned purge still builds a backlog; it drains when writes slow
down.

A count is a cruder bound than a gas budget: the measurements show a factor
of about 14 between a typical and a worst-case block of 128. The two can be
combined: cap an expiry block by its summed deletion bound (D5, from `#meta`)
instead of by a count, and spill on that.

Earlier documents in `arkiv-architecture` leave the question open:
ADR-EXE-004 (expiry first in the block, exact deletion; open), ADR-ECO-003
(price expiry concentration instead of capping it; research), and
`proposals/expiry-model.md` (per-block autonomous expiry with a budget and a
queue, written for the OP Stack design).

## What changes in Arkiv

- The executor deletes exactly at `expiresAt`, in a sweep at the start of
  each block, instead of the producer's `purgeExpired` transaction.
- The expiry schedule moves into state; the producer's pruning database
  (`arkiv-pruning.db`) and `MAX_PURGE_KEYS` go away.
- Genesis gains `expiry_per_block` and `W`.
- Create and extend resolve `E'` and report it in their events.
- The block budget reserves `C × max deletion cost` for the sweep, in place
  of `PURGE_GAS_LIMIT` today or `purge_budget` in the planned purge.

Compared with the planned purge (step 2 of `docs/purging-spec.md`), the sweep
replaces the cursor and the backlog, and create and extend gain the capacity
check. The system step, the removal of the purge transaction and the reserve
are the same work.

## Open questions

1. The values of `C` and `W`. See the measurements below: `C = 128` fits a
   1 s block in the worst case and a 250 ms block only for ordinary entities;
   a 250 ms chain that must bound the worst case needs about 32.
2. Whether spilled blocks, and the probes, should be charged.
3. Whether any application (not the SDK, see below) compares the requested
   expiry with the reported one.
4. Whether to offer the "fail instead of spill" flag from the start.

## Measurements

From the reference implementation, not from Arkiv. Sweep time: producing the
block that deletes a full expiry block, commit included, on durable redb
stores, release build, Ryzen 5 3600 with NVMe. An empty block takes 5 to
8 ms. "Typical" is 4 indexed attributes and a 256-byte body; "worst case" is
the largest entity admitted, 31 indexed 128-byte strings and a 128 KiB value.

| `C` (entities deleted) | Typical | Worst case | Worst case, share of 250 ms | of 1 s |
| --- | --- | --- | --- | --- |
| 16 | 14 ms | 26 ms | 10% | 3% |
| 32 | 13 ms | 50 ms | 20% | 5% |
| 64 | 13 ms | 134 ms | 54% | 13% |
| 128 | 19 ms | 260 ms | 104% | 26% |

Time is linear in `C`: about 0.1 ms per typical entity, 2 ms per worst-case
one. 128 typical entities fit any block period; 128 worst-case ones do not
fit a 250 ms block. Filling a block that way takes 16 MiB of entities written
over at least 26 blocks.

SDK 1.0.1: `createEntity` and `extendEntity` return the `expiresAt` of the
`EntityCreated`/`ExpiryExtended` event, and the SDK's health suite compares
that with the stored `$expiresAt`. Nothing compares either with the lifetime
requested, so a spilled expiry is accepted; the SDK's component and health
suites pass against the reference implementation.

With `C = 128`, a benchmark of 2,000 creates in batches of 16, eight
transactions at a time, all with `ExpirationTime.fromHours(2)`, commits every
create.
