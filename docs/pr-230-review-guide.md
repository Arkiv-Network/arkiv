# Review guide: tokenomics iteration 1 (#230)

One commit on top of `main` (`092e718`, #239). The plan with every decision and its date is
`docs/tokenomics-iteration-1-plan.md`; this guide is the short path through the code.

## 1. Concepts

- **One fee from outside.** An entity transaction is an ordinary gas transaction:
  `gasUsed` = compute + storage + purge gas, priced and settled by EIP-1559 as before.
  `eth_estimateGas` covers it because it runs the same executor. No receipt fields, no new
  RPC; the split exists inside the node only (`GasBreakdown`), ready for the target
  tokenomics.
- **Compute gas** = the transaction's terms (21,000 + calldata, the EIP-7623 floor applied
  once) + each op's cost (`PlaceholderCost`) + the work GolemDB reports in its receipts
  (cost 0 until GolemDB prices calls).
- **Storage gas** = storage units × blocks bought × `capacity_rate` / 10⁶, rounded up once
  per batch. Units follow GolemDB's size rule: a cell is `(8 + name) + (1 + value)` bytes
  plus `c_cell`, an index entry `name + 2 + value` plus `c_idx`. A create buys its size
  for its lifetime, a patch its net growth for the blocks left, an extend its size for the
  added blocks; a shrink, a transfer and a delete buy nothing.
- **Purge gas** is 0: the purge reserve is not collected in this iteration.
- **The pool** is the account at `ARKIV_ADDRESS`. It receives `base_fee × storage_gas`
  (otherwise burned) and the value sent with a successful call. The tip goes to the
  beneficiary as before. Sender debit = burn + tip + pool credit + value.
- **The fee schedule** (`host.arkiv.*`: `capacity_rate`, unit weights, `max_lifetime`)
  comes from the genesis file (`config.arkiv.tokenomics`), is seeded as a store record at
  genesis, and is read at each block's base. A chain without one does not start. A create
  or extend beyond `max_lifetime` (six months) reverts with a typed reason.
- **One frame per transaction.** A batch's ops run on the block's branch inside one frame:
  any failure discards it whole. The gas meter lives outside the frame, so a reverted batch
  still pays for GolemDB's work. A probe (`eth_call`, `eth_estimateGas`) always discards it
  (`ExecEnv::keep_writes`).
- **Never a fault for user input.** Arithmetic overflow anywhere is an `OutOfGas` revert;
  the state layer's receipt sums saturate. An op on a key that holds a non-entity record (an
  account's, GolemDB's own, the fee schedule's) reverts with `NotFound` / `AlreadyExists`:
  the fee schedule's key is new in this PR and is now reserved like the others.

## 2. Main changes, in review order

| Area | Files | What to look at |
| --- | --- | --- |
| Pricing (new crate, pure functions) | `crates/arkiv-tokenomics/src/{schedule,units,storage_time,meter,schedule_record}.rs`, `tests/worked_example.rs` | the size rule, storage time per op, rounding once, the schedule record. `rate.rs` is `$rate` math, unused until the target tokenomics. |
| Interfaces | `crates/arkiv-interfaces/src/{store,statemanager,gas,execution,entity_records}.rs` | `Receipt` with cost and `WriteDetails`; `Metered` results and frames on the state traits; `get_entity_with_meta`; `ExecEnv` / `ExecOutput.gas`; `is_reserved_key` |
| State layer | `crates/arkiv-golemdb-state/src/{view,entities,accounts}.rs`; backends `crates/arkiv-golemdb/src/{lib,convert}.rs`, `crates/arkiv-valkey/src/*` | receipts from every entity read and write and from the minting nonce; frames (`begin`/`merge`/`discard`); the schedule read at the base commit; reserved keys |
| Executor: ops | `crates/arkiv-reth-executor/src/arkiv.rs` | `apply_with_effects` (one frame), `BatchMeter`, per-op compute and storage, `admit_lifetime`, the reserved-key create check |
| Executor: settlement | `crates/arkiv-reth-executor/src/lib.rs` | `arkiv_entity_transact` (builds `ExecEnv`, settles the breakdown), `charge_sender`, `value_in`, `credit_pool`, sender checks and gas-limit guards on view calls and transfers |
| Genesis and startup | `crates/arkiv-reth-chainspec/src/lib.rs`, `crates/arkiv-reth-statemanager/src/genesis.rs`, `bin/arkiv-reth/src/main.rs`, `genesis/{dev,example}.json` | parsing `config.arkiv.tokenomics`, seeding the schedule record, refusing a chain without one |

**Interplay with #239.** Entity transactions deliberately do not use `open_view` /
`close_view`: frames do not nest, and the executor's own per-transaction frame already
discards a probe's writes (comment in `arkiv_entity_transact`). Transfers and view calls do
use #239's frame. #239's e2e test (estimating a delete does not delete) passes.

**Where review is most valuable:** settlement and money conservation (`lib.rs`), probe
isolation, anything that could turn user input into a store fault (a fault fails the
block, and the pooled transaction then fails every block after it), and the overflow paths.

**Visible to users and operators:**

- entity transactions use more gas, since storage is now charged;
- `eth_getBalance(ARKIV_ADDRESS)` grows with storage fees and value sent;
- value sent to `ARKIV_ADDRESS` is no longer ignored: it goes to the pool;
- a lifetime beyond six months reverts;
- the genesis files change, so the genesis hash changes and existing dev chains do not
  carry over.

**Tests to read:**

- executor unit tests in `arkiv.rs` (each pricing rule, overflow, reserved keys);
- host tests at the end of `lib.rs` (conservation, value, probes, sender checks);
- e2e in `bin/arkiv-reth/tests/e2e.rs`, each on memory and GolemDB, printing a fee table
  with `--no-capture`:
  - `the_pool_receives_the_storage_share_and_the_value`
  - `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`
  - `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`
  - `storage_that_does_not_fit_runs_a_create_out_of_gas`
  - `keys_that_hold_no_entity_find_none` (extended with the fee schedule's key)

**Also in this PR, separable:** a by-hand load test (`bin/arkiv-reth/tests/load/`, declared
`test = false`, never in CI) with two harness additions (`NodeBuilder::log_file`,
`ArkivClient::pool_hashes`). It tracks every transaction into a block and reports any lost
or duplicated transaction; see the README's "Load test".

**Postponed by decision:** `arkiv_estimateFee` and receipt fields, `$rate`, the purge
reserve, and pool payouts. The weights are placeholders.

## 3. Size

+10,933 / −493 lines. Inline `#[cfg(test)]` modules count as tests; the harness, the store
conformance suites and the priced test store count as test support.

| | Added | Removed | Share of added |
| --- | ---: | ---: | ---: |
| Production code | 2,065 | 369 | 19% |
| Tests and test support | 7,027 | 120 | 64% |
| Docs (plan, spec, API, load-test design, README) | 1,758 | 3 | 16% |
| Config (Cargo, genesis files) | 83 | 1 | 1% |

Production code by crate:

| Crate | Added | Removed |
| --- | ---: | ---: |
| `arkiv-reth-executor` | 596 | 196 |
| `arkiv-tokenomics` (new) | 553 | 0 |
| `arkiv-interfaces` | 466 | 37 |
| `arkiv-golemdb-state` | 181 | 58 |
| `arkiv-valkey`, `arkiv-golemdb` | 138 | 60 |
| `arkiv-reth-statemanager`, `arkiv-reth-chainspec`, `bin/arkiv-reth`, other | 131 | 18 |

The largest test files are the load test (`run.rs` 909, `plan.rs` 523), the executor's unit
tests (`arkiv.rs` 818 inline), `e2e.rs` (638), the host tests in `lib.rs` (533) and the store
conformance suite (441).
