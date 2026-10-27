# E2E load test: track every transaction under load, stop at the first broken invariant

## Problem

Under load, a node has twice lost a transaction: it disappeared from the pool and never
reached a block. Both times, a full `cargo nextest run --workspace` ran 2–4× slower than
usual (130 s and 64 s, against about 33 s), and the transaction belonged to the last test
still running:

- `no_transaction_is_included_twice_over_a_live_node`: `no receipt for probe tx 21 (…):
  transaction was not confirmed within the timeout`.
- `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`: the first create, sent at
  block 0, `no receipt … in 60s: submitted at block 0, chain now at block 241, tx in pool:
  absent (status: 0x0 pending / 0x0 queued)`.

Both tests pass when run alone, and so do repeated full runs. The cause is unknown:

- The node log is deleted when the `Node` drops (`crates/arkiv-harness/src/node.rs`,
  `Drop`); a failing test prints only its last 150 lines, which show empty blocks being
  built.
- The symptom fits the payload builder rejecting the transaction as invalid (reth then
  removes it from the pool), or the pool revalidating it against stale state. Neither can
  be confirmed or ruled out, and neither can whether it predates the tokenomics work.

The existing e2e tests notice a lost transaction only when a receipt times out, a minute
later, and they keep no record of what happened to it.

## Goal

An e2e test that sends a large, varied, reproducible stream of transactions to a live node.
It tracks every transaction from submission to inclusion and checks invariants as each
block arrives. Every run writes a report, and the report alone tells whether the run was
clean: at the first violation the test stops and the report holds everything needed to
analyse it.
This is not fuzzing with shrinking, but close to it in practice: each run is random, each
failure replays from its seed, and a failure shows the state at the moment it went wrong.

## Design

### Generator

- Seeded RNG. The seed is printed at the start and can be set through `ARKIV_LOAD_SEED` to
  replay a run.
- Many senders at once (the dev chain funds 100 keys), each with explicit nonces, so the
  test, not the client's fillers, decides the order.
- A weighted mix:
  - entity batches of 1 to n ops (create, patch, extend, transfer, delete), against keys
    the model knows, predicted from the minting nonce;
  - plain value transfers (faucet traffic);
  - mined view calls;
  - transactions meant to fail, each with a known outcome: a business-rule revert (a patch
    of a deleted entity), out of gas (storage that does not fit), and a gas limit below
    the charge.
- Load shapes: bursts; back-to-back sends, the next one right after the previous receipt
  (the race `golemdb_back_to_back_transactions_all_land` covers); `eth_estimateGas`
  interleaved with sends, so estimates run against a busy chain; later, a restart in the
  middle on GolemDB.

### Ledger

One record per transaction: hash, sender, nonce, kind, the outcome the model expects, the
block it was sent at, and a timeline (sent, seen in the pool, included in block N, receipt
status and gas).

### Block watcher and invariants

A task runs beside the generator. It reads each new block with its full transaction list and
checks, failing at once:

1. **Nothing lost.** Every sent transaction is in exactly one block or still in the pool
   (`txpool_content`). One that is in neither is a failure the moment it is missed, not a
   minute later.
2. **No duplicates, no gaps.** No transaction is included twice, and each sender's nonces
   land contiguously.
3. **Outcomes.** Each receipt's status matches the model's expectation.
4. **Entity state.** For the keys a block touched (a sample, under load), owner, expiry and
   attributes from `arkiv_getEntity` match the shadow model.
5. **Money.** Per block, across every tracked account (senders, recipients, the
   beneficiary, the pool at `ARKIV_ADDRESS`), total supply falls by exactly the burned base
   fee; the pool rises by the storage share plus the value sent, and the beneficiary by
   the tips.
6. **Clean end.** The pool is empty, and every transaction is accounted for.
7. **Nothing unknown.** Transactions the run did not send (the sequencer's purge calls)
   are listed; one from a run sender is a failure.

### Run report

Every run writes one JSON report into a temporary directory of its own, for example
`$TMPDIR/arkiv-load-<seed>-<unix time>-<pid>/report.json`, and prints the path at the start. The
directory is kept after the run, pass or fail, and holds the node log beside the report.

The report alone answers whether the run was clean, without the test's output:

- `outcome` is `running` from the moment the run starts, then `passed` or `failed`. A report
  still saying `running` means the run died without finishing, a crash or a kill, which is
  itself a finding. `aborted` means the test panicked outside an invariant check, an RPC
  failure for example; the panic message is in the test's output.
- `violations` lists every broken invariant: its name (`lost`, `duplicate`, `nonce_gap`,
  `unexpected_outcome`, `entity_mismatch`, `unknown_tx`, `unclean_end`, and `money` once
  step 3 exists), the block, and the transactions and keys involved. Empty when `passed`.
- `summary` counts what happened: sent, included, reverted as expected, still pending,
  lost, duplicated, and the blocks covered. A clean run shows `lost: 0` and `duplicated: 0`
  explicitly.
- `seed` and the run's settings (transaction count, senders, mix, store), so the run can be
  replayed, and `planDigest`, a fingerprint of every planned transaction: two runs with
  the same seed and sizes show the same digest, whatever their timing.

Beside it, `transactions.jsonl` lists every transaction on every run, pass or fail, one JSON
object per line: its ops in full, the expected and actual outcome, the block and position,
`gasUsed`, the effective gas price and the fee in wei.

On a failure the report also carries, for analysis:

- every transaction's timeline;
- the last N blocks with their transactions;
- the pool's contents;
- the shadow model next to the actual state, for the affected keys.

The node log in the same directory is plain text, with transaction-pool and payload-builder
logging at trace level and Arkiv's own at debug, so a lost transaction's report comes with
the node's account of why it was dropped. It runs to about 0.8 MB per 300 transactions.

### Harness changes

Done with step 1:

- `NodeBuilder::log_file` puts the node log where a test wants it and keeps it after the
  node drops (`crates/arkiv-harness/src/node.rs`).
- `ArkivClient::pool_hashes` reads `txpool_content` into the pool's pending and queued
  hashes (`crates/arkiv-harness/src/client.rs`).
- Per-node log levels needed no change: `NodeBuilder::arg` passes `--log.stdout.filter`.

### Sizes and how it runs

Both sizes are outside CI: the tests live in `bin/arkiv-reth/tests/load/`, a target declared
`test = false`, so neither `cargo test` nor `cargo nextest run` picks it up, not even CI's
`-- --ignored` step, and `#[ignore]`d besides. They run by hand, with `--test load`.

- **Small** (the default): a few hundred transactions in about 30 s.
- **Soak:** sized by `ARKIV_LOAD_TXS` or a time limit, to run in a loop before a release
  or overnight.

The repository's README gets a short section on running it: the command for each size,
`ARKIV_LOAD_SEED` to replay a run, and where to find the report and what `outcome` means.

## Steps

Each step is useful on its own.

1. *Done (2026-10-27).* **Ledger, lost/duplicate/gap invariants, run report, keeping the
   log.** A simple mix:
   creates and value transfers from many senders, in bursts and back to back. *Done when:*
   a run of 1,000 transactions passes, and a deliberately dropped transaction (one the
   test signs but never sends, entered in the ledger) ends the run with `outcome: failed`
   and a `lost` violation naming it. The README section exists.
2. *Done (2026-10-27).* **Shadow entity model and expected outcomes,** with the full op mix
   and the transactions meant to fail. *Done when:* every receipt status and every sampled
   entity matches the model over a seeded run, and replaying the seed reproduces the same
   sequence.
   Implemented so that every outcome is exact: an entity is only ever touched by the sender
   that created it, and a transfer hands it to a sink address that never sends, so a
   sender's nonce order alone decides each outcome. Expiries are relative and resolved at the
   inclusion block. Not yet: `eth_estimateGas` interleaved with sends.
3. **Money conservation per block.**
4. **Soak mode, restarts, GolemDB.**

## Decided

- Both sizes run outside CI, by hand; the README says how.
- Each run writes its report into a temporary directory of its own.
- The report alone says whether the run was clean or what broke.
- Whether a finding, such as a lost transaction, becomes an issue is decided by hand. The
  run's report, with the node log beside it, is what gets attached to that issue, so it
  must be self-contained: readable without the test's output or the machine it ran on.
