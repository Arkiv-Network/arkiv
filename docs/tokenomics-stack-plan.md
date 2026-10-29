# Splitting #230: two bug fixes and a tokenomics stack

Agreed on 2026-10-29: finish `main` without tokenomics and deploy a test chain from it, then
rebuild #230 as a stack of small PRs. This note proposes the split; please comment on the
boundaries before any code moves.

## 1. Two bugs

| Issue | Bug | Fix when |
| --- | --- | --- |
| #263 | Extend, transfer and delete read the whole entity, extend and transfer three times; extend and transfer then rewrite all its cells. An extend of a 100 KiB entity reads 300 KiB and writes 100 KiB, at a price that doesn't depend on the size | now, on `main` |
| #262 | An entity op batch doesn't run in its own frame: no checkpoint is taken before the batch | before GolemDB metering (T5) |

**#262 doesn't affect `main` as long as gas is a fixed cost per op.** A fixed cost depends
only on the op itself (its type, bytes and indexed attributes, all in the calldata). So the
executor knows before writing anything whether the batch fits into the gas limit, and out
of gas always happens while nothing has been written. With GolemDB metering, an op's cost
also depends on the stored state, and GolemDB reports it only after the call has run.
Whether a batch runs out of gas is then known only after its writes have reached the
branch, and a revert has to undo them.

## 2. Before the test chain goes up

- **Lifetime cap:** `main` has no upper bound on an entity's lifetime, and the price
  doesn't depend on it. If the test chain is still running when tokenomics lands, it holds
  entities the six-month cap would have refused. On #230 the cap is
  `storage_time::within_max_lifetime` and the `ExpiryBeyondMaxLifetime` revert.
- **Reset or activation block:** tokenomics changes the gas rules. Either the test chain is
  reset when it lands, or the new rules need an activation block, which is extra code and
  review.

## 3. The tokenomics stack

Each PR builds and passes CI on its own, and brings its tests and its part of the spec.

| PR | Content | Depends on |
| --- | --- | --- |
| T1 | **Pricing as pure functions.** The `arkiv-tokenomics` crate (storage units, storage time, `$rate`, gas meter) and the size types it uses from `arkiv-interfaces` (`RecordMeta`, `WriteDetails`, GolemDB's size rule). The worked example as a test. No store, no node | – |
| T2 | **Stores report sizes.** A read returns the record's size (`#meta`), a write returns what it changed (`WriteDetails`). Reference store, GolemDB, Valkey and the state layer, with the store conformance tests. No prices | T1 |
| T3 | **The executor charges storage; parameters are constants.** Gas = compute + storage. Compute = 21,000 + calldata + fixed cost per op (`PlaceholderCost`), at least the EIP-7623 floor. Storage = units × blocks bought × rate: a create buys its lifetime, an extend the added blocks, a patch its growth; no refunds. Settlement as on `main`: base fee burned, tip to the block producer | T2 |
| T4 | **Parameters from the genesis file** (part of #250). Genesis → GolemDB's metering model → pricing snapshot per block → `FeeSchedule`. The node refuses to start without the host weights | T3 |
| T5 | **GolemDB metering** (rest of #250). Op costs from GolemDB's receipts (`OpCost::FromStore`), each op metered as it runs, a revert pays for the work it undid. Spans interfaces, GolemDB, state layer and executor, so it will need splitting further | T4, #262 |
| T6 | **Pool.** The storage share and the call's `value` go to `ARKIV_ADDRESS`. On `main`, an entity transaction's `value` stays with the sender | T3 |

```
T1 ── T2 ── T3 ─┬─ T4 ─┬─ T5
                │ #262 ┘
                └─ T6
```

**Proposed Mumbai cut: T1 to T3,** plus T4 if the parameters must come from the genesis file
at launch. After the cut, T4 changes where the parameters come from, T5 the compute price,
and T6 where the storage share and `value` go.

Not in the stack: the load test (`bin/arkiv-reth/tests/load/`), the SDK experiment script,
and the dev-container change.

## 4. How we build it

- **#230 at `7fdb0fb` is the reference.** At the end of the stack the code behaves the same,
  or the PR says where and why it differs. The worked example and the e2e tests move with
  the code.
- **Split by topic, not by commit.** #230's commits follow the order the work was done.
- **Boundaries first.** Once we agree on this table, AI does the mechanical part: moving code
  and tests, fixing the build, a first review pass.
- **At most about 500 lines of production code per PR.**

## 5. Size of #230

Production code by area, counted from where #230 branched off `main`. Test modules inside
`src/`, the conformance suites and the test store count as tests.

| Area | Added | Removed |
| --- | ---: | ---: |
| `arkiv-tokenomics` (new) | 500 | 0 |
| `arkiv-interfaces` | 584 | 40 |
| `arkiv-reth-executor` | 733 | 221 |
| GolemDB and the state layer | 422 | 110 |
| Valkey | 110 | 36 |
| chainspec, node, genesis | 94 | 17 |
| **Production code** | **2,443** | **424** |
| Tests | 7,441 | 163 |
| Docs | 2,735 | 17 |
| SDK script, dev container | 626 | 3 |
| Config | 184 | 15 |

These counts include #263, which #230 already contains.

## Open questions

1. Are these the right boundaries? How should T5 be split?
2. Should storage sizes come from GolemDB (T2, as in #230) or be counted in Arkiv from the
   entity's cells? Counting in Arkiv saves T2's store changes, but an extend then has to read
   the whole entity again (what #263 removes), and Arkiv duplicates GolemDB's size rule.
3. Lifetime cap on `main` before the test chain?
4. Reset the test chain when tokenomics lands, or an activation block?
5. Is T1 to T3 enough for Mumbai, or is T4 needed too?
6. Who reviews which PR?
