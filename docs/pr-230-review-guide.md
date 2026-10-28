# Review guide: tokenomics iteration 1 (#230)

One commit on top of `main` (#239). Every decision, with its date, is in
`docs/tokenomics-iteration-1-plan.md`. This guide is the short path through the code.

- [Review split](#review-split)
- [1. Concepts](#1-concepts)
- [2. Changes](#2-changes)
- [3. How GolemDB's metering reaches the fee](#3-how-golemdbs-metering-reaches-the-fee)
- [4. Size](#4-size)

## Review split

One reviewer for GolemDB metering, one for the integration into genesis and execution. Both
read `crates/arkiv-golemdb-state`: the first checks the numbers it reports, the second its
rollback rules. Settlement goes to the second.

- **GolemDB metering into fees:**
  - what each store call reports (`Receipt`, `WriteDetails`, `#meta`), and that MemStore,
    Valkey and GolemDB agree (`crates/arkiv-interfaces/src/store.rs`, `store/conformance.rs`,
    `crates/arkiv-golemdb/src/convert.rs`);
  - how the state layer adds them up (`metering_tests.rs`);
  - the pricing in `crates/arkiv-tokenomics`: size rule, storage time, rounding;
  - the executor's gas meter (`BatchMeter` in `arkiv.rs`);
  - section 3, and the pricing rules in `docs/tokenomics-spec.md`.
- **Integration into genesis and execution:**
  - the fee schedule, from the genesis file to the store record, and the node refusing to
    start without one (`arkiv-reth-chainspec`, `arkiv-reth-statemanager/src/genesis.rs`,
    `bin/arkiv-reth/src/main.rs`);
  - one rollback frame per transaction, `eth_call` and `eth_estimateGas` discarding theirs,
    and how this fits with #239 (`apply_with_effects`, `arkiv_entity_transact`);
  - every path where a user's transaction could cause an error that fails the block instead
    of a revert (see #240);
  - settlement: `charge_sender`, `value_in`, `credit_pool`, the sender checks (`lib.rs`).

Where the time might go, as a rough guide only; follow what you find:

| Reviewer | Most time | Skim |
| --- | --- | --- |
| GolemDB metering | `arkiv-tokenomics`; `arkiv-interfaces/src/store.rs` with the store conformance suite | `arkiv-golemdb-state` (receipts), `arkiv-golemdb/src/convert.rs`, `BatchMeter` in `arkiv-reth-executor/src/arkiv.rs` |
| Integration | `arkiv-reth-executor`: `arkiv.rs` (ops) and `lib.rs` (settlement) | `arkiv-golemdb-state` (frames, reserved keys), `arkiv-reth-chainspec`, `arkiv-reth-statemanager/src/genesis.rs`, `bin/arkiv-reth/src/main.rs` |

The load test (`bin/arkiv-reth/tests/load/`) is optional reading: it changes no production
code.

## 1. Concepts

- **One fee, as for any transaction.** `gasUsed` = compute + storage + purge gas, charged
  with EIP-1559 as before. `eth_estimateGas` includes all three. No new receipt fields or
  RPC methods; the split exists only inside the node (`GasBreakdown`).
- **Compute gas** = 21,000 + calldata (with the EIP-7623 floor) + each op's cost
  (`PlaceholderCost`) + the cost GolemDB reports for its reads and writes (0 for now).
- **Storage gas** = storage units × blocks × `capacity_rate` / 10⁶, rounded up once per
  transaction. Units follow GolemDB's size rule: a cell is `(8 + name) + (1 + value)` bytes
  plus `c_cell`, an index entry `name + 2 + value` bytes plus `c_idx`.
  - A create pays for its size over its lifetime.
  - A patch pays for its growth over the blocks left.
  - An extend pays for the entity's size over the added blocks.
  - A shrinking patch, a transfer and a delete pay nothing.
- **Purge gas** is always 0 for now.
- **The pool** is the account at `ARKIV_ADDRESS`. It gets `base_fee × storage_gas`, which
  would otherwise be burned, plus any value sent with a successful call. The tip goes to the
  block producer as before. So the sender pays the burned fee + the tip + the pool's share +
  the value.
- **The fee schedule** (`capacity_rate`, the unit weights, `max_lifetime`) is set in the
  genesis file (`config.arkiv.tokenomics`), stored as a record at genesis, and read at the
  start of each block. Without one the node does not start. A create or extend beyond
  `max_lifetime` (six months) reverts.
- **All or nothing per transaction.** A transaction's ops run inside one rollback frame; if
  any op fails, all are undone. The gas meter is outside the frame, so a reverted
  transaction still pays for the GolemDB work it did. `eth_call` and `eth_estimateGas`
  always roll back.
- **User input never fails a block.** An arithmetic overflow becomes an `OutOfGas` revert.
  An op on a key that holds something other than an entity (an account, the fee schedule,
  GolemDB's own records) reverts with `NotFound` or `AlreadyExists`.

## 2. Changes

Suggested reading order:

| Area | Files | Look at |
| --- | --- | --- |
| Pricing (new crate, no I/O) | `crates/arkiv-tokenomics/src/{schedule,units,storage_time,meter,schedule_record}.rs`, `tests/worked_example.rs` | size rule, storage time per op, rounding, the schedule record. `rate.rs` is not used yet. |
| Interfaces | `crates/arkiv-interfaces/src/{store,statemanager,gas,execution,entity_records}.rs` | `Receipt` (cost and bytes), rollback frames on the state traits, `get_entity_with_meta`, `ExecEnv`, `ExecOutput.gas`, `is_reserved_key` |
| State layer | `crates/arkiv-golemdb-state/src/{view,entities,accounts}.rs`; `crates/arkiv-golemdb/src/{lib,convert}.rs`; `crates/arkiv-valkey/src/*` | a receipt from every entity read and write, frames, reading the schedule |
| Executor: ops | `crates/arkiv-reth-executor/src/arkiv.rs` | `apply_with_effects`, `BatchMeter`, compute and storage per op, `admit_lifetime` |
| Executor: settlement | `crates/arkiv-reth-executor/src/lib.rs` | `arkiv_entity_transact`, `charge_sender`, `value_in`, `credit_pool`, the sender checks and gas-limit checks |
| Genesis and startup | `crates/arkiv-reth-chainspec/src/lib.rs`, `crates/arkiv-reth-statemanager/src/genesis.rs`, `bin/arkiv-reth/src/main.rs`, `genesis/{dev,example}.json` | reading `config.arkiv.tokenomics`, storing the schedule, refusing to start without one |

**#239.** Entity transactions do not use #239's `open_view` / `close_view`: rollback frames
cannot be nested, and the executor already opens one per transaction and rolls it back for
`eth_call` and `eth_estimateGas` (see the comment in `arkiv_entity_transact`). Transfers and
view calls do use #239's frame.

**What users notice:**

- entity transactions cost more gas, because storage is charged;
- the balance of `ARKIV_ADDRESS` grows;
- value sent to `ARKIV_ADDRESS` goes to the pool; before, it was ignored;
- a lifetime beyond six months reverts;
- the genesis files change, so existing dev chains cannot be reused.

**Tests to read first:**

1. `golemdbs_work_reaches_settlement` (`crates/arkiv-reth-executor/src/lib.rs`): GolemDB's
   cost (114 gas on a test store that charges per call) shows up in `gasUsed`, in what the
   sender pays and in the tip.
2. `a_create_buys_its_attributes_and_their_index_entries`
   (`crates/arkiv-reth-executor/src/arkiv.rs`): storage gas worked out by hand from the size
   rule, index entries included.
3. `the_pool_gets_the_storage_share_and_the_value` (`lib.rs`): the sender pays the burned
   fee + the tip + the pool's share + the value.
4. `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`
   (`bin/arkiv-reth/tests/e2e.rs`): signed transactions against a running node, on memory and
   on GolemDB; run with `--no-capture` to see each fee split.
5. `keys_that_hold_no_entity_find_none` (`e2e.rs`, on GolemDB): an op on an account's key,
   the fee schedule's key or a GolemDB record reverts, and the chain keeps producing blocks.

Then:

- the unit tests in `arkiv.rs`: each pricing rule, overflow, reserved keys;
- the tests at the end of `lib.rs`: what the sender pays, value, `eth_call` and
  `eth_estimateGas`, sender checks;
- three more e2e tests, on memory and GolemDB:
  `the_pool_receives_the_storage_share_and_the_value`,
  `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`,
  `storage_that_does_not_fit_runs_a_create_out_of_gas`.

**Also in the PR:** a load test, run by hand and never in CI (`bin/arkiv-reth/tests/load/`),
with two helpers in `arkiv-harness`. It checks that every transaction it sends lands in
exactly one block. See the README's "Load test".

**Not in this iteration:** `arkiv_estimateFee`, the fee split in receipts, `$rate`, the
purge reserve, paying validators from the pool. The weights are placeholders.

## 3. How GolemDB's metering reaches the fee

GolemDB's metering reaches the fee two ways: the cost it reports becomes compute gas, and
the bytes it reports become storage gas.

| Layer | Cost → compute gas | Bytes → storage gas |
| --- | --- | --- |
| Store | `crates/arkiv-interfaces/src/store/priced.rs`: a test store that charges a different amount per call kind, so each total shows which calls it includes. Real GolemDB reports 0 today. | `run_write_details`, `a_read_reports_the_records_size` (`store/conformance.rs`): MemStore, Valkey and GolemDB report the same bytes. |
| State layer (`crates/arkiv-golemdb-state/src/metering_tests.rs`) | `a_create_reports_its_existence_read_and_its_write`, `an_update_reports_its_read_and_its_patch`, `advancing_the_minting_nonce_reports_one_read_and_one_write`, `a_discarded_frame_keeps_what_it_spent` | `pricing_metadata_survives_reads_writes_and_aggregation`, `no_op_and_attribute_removal_report_only_their_actual_effects` |
| Executor (`crates/arkiv-reth-executor/src/arkiv.rs`) | `compute_adds_the_terms_the_ops_and_golemdbs_work`, `a_revert_pays_for_the_golemdb_work_it_undid`, `golemdbs_work_can_run_a_batch_out_of_gas` | `a_create_buys_its_size_until_expiry`, `a_patch_buys_only_its_growth`, `a_patch_that_grows_and_shrinks_buys_its_net_growth`, `an_extend_buys_only_the_added_blocks`, `storage_gas_is_rounded_once_per_batch` |
| Settlement (`crates/arkiv-reth-executor/src/lib.rs`) | `golemdbs_work_reaches_settlement`, `a_reverted_transaction_pays_its_golemdb_work` | `the_pool_gets_the_storage_share_and_the_value` |
| Running node (`bin/arkiv-reth/tests/e2e.rs`) | none: GolemDB charges 0 | `the_pool_receives_the_storage_share_and_the_value`, `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`, `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`, `storage_that_does_not_fit_runs_a_create_out_of_gas` |

Not covered yet: a non-zero GolemDB cost reaching the fee on a real GolemDB, because GolemDB
does not charge yet. Cost is covered with the test store, bytes with real GolemDB.

## 4. Size

+10,933 / −493 lines. Test modules inside `src/` count as tests; so do `arkiv-harness`, the
store conformance suites and the test store that charges per call.

| | Added | Removed | Share of added |
| --- | ---: | ---: | ---: |
| Production code | 2,065 | 369 | 19% |
| Tests | 7,027 | 120 | 64% |
| Docs | 1,758 | 3 | 16% |
| Config (Cargo, genesis files) | 83 | 1 | 1% |

Production code by crate:

| Crate | Added | Removed |
| --- | ---: | ---: |
| `arkiv-reth-executor` | 596 | 196 |
| `arkiv-tokenomics` (new) | 553 | 0 |
| `arkiv-interfaces` | 466 | 37 |
| `arkiv-golemdb-state` | 181 | 58 |
| `arkiv-valkey`, `arkiv-golemdb` | 138 | 60 |
| others | 131 | 18 |

The largest test files: the load test (`run.rs` 909, `plan.rs` 523), the unit tests in
`arkiv.rs` (818), `e2e.rs` (638), the tests in `lib.rs` (533), and the store conformance
suite (441).
