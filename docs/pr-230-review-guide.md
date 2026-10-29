# Review guide: tokenomics iteration 1 (#230)

State of 2026-10-29, at `7fdb0fb`. Every decision, with its date, is in
`docs/tokenomics-iteration-1-plan.md`. This guide is the short path through the code.

- [What is in the PR](#what-is-in-the-pr)
- [Review split](#review-split)
- [1. Concepts](#1-concepts)
- [2. Changes](#2-changes)
- [3. How GolemDB's metering reaches the fee](#3-how-golemdbs-metering-reaches-the-fee)
- [4. Size](#4-size)

## What is in the PR

Thirteen commits on top of `main` (#239), in three groups:

| Group | Commits | What |
| --- | --- | --- |
| Iteration 1, as first reviewed | `8e4ee9d` … `5ea8641` | fee split into compute, storage and purge gas; storage priced by size and lifetime; the pool at `ARKIV_ADDRESS`; one rollback frame per transaction; review fixes |
| GolemDB pricing (#250, Krzysztof Fonał) | `4a11946` | the fee schedule and GolemDB's own weights from the genesis file into GolemDB's metering model, read back as a pricing snapshot per block |
| Follow-ups | `a05d342`, `1890e0f`, `7fdb0fb` | on GolemDB an op costs what GolemDB reports, not the placeholder; distinct weights in `example.json` with a test that each reaches the node; extend and transfer write only the system cells they change |
| Not node code | `f63c81b`, `635da74`, `8329e4c` | bun in the dev container; an SDK script that measures what each op costs (`kurtosis/integration/experiment/`) |

## Review split

One reviewer for GolemDB metering, one for the integration into genesis and execution. Both
read `crates/arkiv-golemdb-state`: the first checks the numbers it reports, the second its
rollback rules. Settlement goes to the reviewer focussing on execution.

- **GolemDB metering into fees:**
  - what each store call reports (`Receipt`, `WriteDetails`, `#meta`), and that MemStore,
    Valkey and GolemDB agree (`crates/arkiv-interfaces/src/store.rs`, `store/conformance.rs`,
    `crates/arkiv-golemdb/src/convert.rs`);
  - the pricing model GolemDB keeps and returns (`crates/arkiv-golemdb/src/lib.rs`,
    `pricing_snapshot`);
  - how the state layer adds the receipts up (`metering_tests.rs`), and which cells a write
    passes to GolemDB (`update` in `crates/arkiv-golemdb-state/src/entities.rs`);
  - the pricing in `crates/arkiv-tokenomics`: size rule, storage time, rounding;
  - the executor's gas meter (`BatchMeter` in `arkiv.rs`);
  - section 3, and the pricing rules in `docs/tokenomics-spec.md`.
- **Integration into genesis and execution:**
  - the weights, from the genesis file into GolemDB's metering model, and the node refusing
    to start without usable host weights (`arkiv-reth-chainspec`,
    `bin/arkiv-reth/src/main.rs`, `genesis/{dev,example}.json`);
  - the pricing snapshot captured once per block (`BlockPricing` in
    `crates/arkiv-reth-executor/src/lib.rs`);
  - one rollback frame per transaction, `eth_call` and `eth_estimateGas` discarding theirs,
    and how this fits with #239 (`apply_with_effects`, `arkiv_entity_transact`);
  - every path where a user's transaction could cause an error that fails the block instead
    of a revert (see #240);
  - settlement: `charge_sender`, `value_in`, `credit_pool`, the sender checks (`lib.rs`).

Where the time might go, as a rough guide only; follow what you find:

| Reviewer | Most time | Skim |
| --- | --- | --- |
| GolemDB metering | `arkiv-tokenomics`; `arkiv-interfaces/src/store.rs` with the store conformance suite; `arkiv-golemdb/src/lib.rs` | `arkiv-golemdb-state` (receipts), `arkiv-golemdb/src/convert.rs`, `BatchMeter` in `arkiv-reth-executor/src/arkiv.rs` |
| Integration | `arkiv-reth-executor`: `arkiv.rs` (ops) and `lib.rs` (pricing snapshot, settlement) | `arkiv-golemdb-state` (frames, reserved keys), `arkiv-reth-chainspec`, `bin/arkiv-reth/src/main.rs` |

The load test (`bin/arkiv-reth/tests/load/`) and the SDK script
(`kurtosis/integration/experiment/`) are optional reading: they change no production code.

## 1. Concepts

- **One fee, as for any transaction.** `gasUsed` = compute + storage + purge gas, charged
  with EIP-1559 as before. `eth_estimateGas` includes all three. No new receipt fields or
  RPC methods; the split exists only inside the node (`GasBreakdown`).
- **Compute gas** = 21,000 + calldata (with the EIP-7623 floor) + what each op costs. On
  GolemDB that is what GolemDB reports for its reads and writes, priced with the weights of
  the genesis file (`OpCost::FromStore`). MemStore and Valkey price nothing, so there the
  provisional `PlaceholderCost` stands in (`OpCost::Placeholder`); `main` picks one from the
  store at startup.
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
- **The weights** are in the genesis file, `config.arkiv.golemdb.metering.weights`:
  GolemDB's 32 engine weights (`write.*`, `read.*`) and Arkiv's six host weights
  (`host.arkiv.capacity_rate`, the four unit weights, `host.arkiv.max_lifetime`). GolemDB
  stores them as its metering model at genesis; the executor reads the host weights back
  from GolemDB's pricing snapshot once per block. A missing or zero host weight stops the
  node; a missing engine weight leaves that operation unpriced, never free. A create or
  extend beyond `max_lifetime` (six months) reverts.
- **All or nothing per transaction.** A transaction's ops run inside one rollback frame; if
  any op fails, all are undone. The gas meter is outside the frame, so a reverted
  transaction still pays for the GolemDB work it did. `eth_call` and `eth_estimateGas`
  always roll back.
- **User input never fails a block.** An arithmetic overflow becomes an `OutOfGas` revert.
  An op on a key that holds something other than an entity (an account, GolemDB's own
  records) reverts with `NotFound` or `AlreadyExists`.
- **An op reads and writes only what it needs.** Extend, transfer and delete read the
  entity's system fields, not its attributes, and extend and transfer write only the system
  cells they change. A patch still reads the whole entity and passes every cell to GolemDB,
  so it pays GolemDB's admission for unchanged cells too: about 100,000 gas extra on an
  entity with 100 KiB. Open.

## 2. Changes

Suggested reading order:

| Area | Files | Look at |
| --- | --- | --- |
| Pricing (new crate, no I/O) | `crates/arkiv-tokenomics/src/{schedule,units,storage_time,meter}.rs`, `tests/worked_example.rs` | size rule, storage time per op, rounding, `FeeSchedule::from_model`. `rate.rs` is not used yet. |
| Interfaces | `crates/arkiv-interfaces/src/{store,statemanager,gas,execution,entity_records}.rs` | `Receipt` (cost and bytes), `PricingSnapshot`, rollback frames on the state traits, `get_entity_with_meta` and `get_entity_system_fields`, `OpCost`, `ExecEnv`, `ExecOutput.gas`, `is_reserved_key` |
| GolemDB store | `crates/arkiv-golemdb/src/{lib,convert}.rs` | the metering model from the genesis, `pricing_snapshot`, receipts |
| State layer | `crates/arkiv-golemdb-state/src/{view,entities,accounts}.rs`; `crates/arkiv-valkey/src/*` | a receipt from every entity read and write, frames, the system-cell write path in `update` |
| Executor: ops | `crates/arkiv-reth-executor/src/arkiv.rs` | `apply_with_effects`, `BatchMeter`, compute and storage per op, `mutate` and `EntityRead`, `admit_lifetime` |
| Executor: settlement | `crates/arkiv-reth-executor/src/lib.rs` | `BlockPricing`, `arkiv_entity_transact`, `charge_sender`, `value_in`, `credit_pool`, the sender checks and gas-limit checks |
| Genesis and startup | `crates/arkiv-reth-chainspec/src/{lib,parser}.rs`, `crates/arkiv-reth-statemanager/src/genesis.rs`, `bin/arkiv-reth/src/{main,node}.rs`, `genesis/{dev,example}.json` | reading `config.arkiv.golemdb.metering`, refusing to start without usable host weights, choosing `OpCost` from the store |

**#239.** Entity transactions do not use #239's `open_view` / `close_view`: rollback frames
cannot be nested, and the executor already opens one per transaction and rolls it back for
`eth_call` and `eth_estimateGas` (see the comment in `arkiv_entity_transact`). Transfers and
view calls do use #239's frame.

**What users notice:**

- entity transactions cost more gas, because storage is charged;
- on GolemDB the compute part follows the genesis weights, not the placeholder;
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
4. `every_weight_of_the_example_genesis_file_reaches_the_node` (`bin/arkiv-reth/src/main.rs`):
   all 38 weights of `example.json`, each with a value of its own, arrive in GolemDB's
   pricing snapshot under their own names, and each host weight in its `FeeSchedule` field.
5. `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`
   (`bin/arkiv-reth/tests/e2e.rs`): signed transactions against a running node, on memory and
   on GolemDB; run with `--no-capture` to see each fee split.
6. `ops_that_leave_the_attributes_alone_cost_the_same_however_large_the_entity` (`e2e.rs`, on
   GolemDB): extend, transfer and delete cost the same compute on an entity with 16 bytes
   and on one with 100 KiB.
7. `keys_that_hold_no_entity_find_none` (`e2e.rs`, on GolemDB): an op on an account's key or
   a GolemDB record reverts, and the chain keeps producing blocks.

Then:

- the unit tests in `arkiv.rs`: each pricing rule, overflow, reserved keys;
- the tests at the end of `lib.rs`: what the sender pays, value, `eth_call` and
  `eth_estimateGas`, sender checks;
- `both_backends_use_and_expose_the_persisted_pricing_model`
  (`crates/arkiv-golemdb/src/tests.rs`): the pricing model on disk and in memory, across a
  commit and a reopen;
- three more e2e tests, on memory and GolemDB:
  `the_pool_receives_the_storage_share_and_the_value`,
  `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`,
  `storage_that_does_not_fit_runs_a_create_out_of_gas`.

**Also in the PR, never run in CI:** a load test (`bin/arkiv-reth/tests/load/`, two helpers
in `arkiv-harness`), which checks that every transaction it sends lands in exactly one
block; and the SDK script (`kurtosis/integration/experiment/tokenomics.ts`), which measures
each op's gas, fee and how many fit in a block (`docs/sdk-next-testing.md`).

**Not in this iteration:** `arkiv_estimateFee`, the fee split in receipts, `$rate`, the
purge reserve, paying validators from the pool, a patch that passes GolemDB only the cells
it changes. The weights are not calibrated.

## 3. How GolemDB's metering reaches the fee

GolemDB's metering reaches the fee two ways: the cost it reports becomes compute gas, and
the bytes it reports become storage gas.

| Layer | Cost → compute gas | Bytes → storage gas |
| --- | --- | --- |
| Store | `crates/arkiv-interfaces/src/store/priced.rs`: a test store that charges a different amount per call kind, so each total shows which calls it includes. GolemDB prices every call with the genesis weights (`both_backends_use_and_expose_the_persisted_pricing_model`). | `run_write_details`, `a_read_reports_the_records_size` (`store/conformance.rs`): MemStore, Valkey and GolemDB report the same bytes. |
| State layer (`crates/arkiv-golemdb-state/src/metering_tests.rs`) | `a_create_reports_its_existence_read_and_its_write`, `an_update_reports_its_read_and_its_patch`, `advancing_the_minting_nonce_reports_one_read_and_one_write`, `a_discarded_frame_keeps_what_it_spent` | `pricing_metadata_survives_reads_writes_and_aggregation`, `no_op_and_attribute_removal_report_only_their_actual_effects` |
| Executor (`crates/arkiv-reth-executor/src/arkiv.rs`) | `compute_adds_the_terms_the_ops_and_golemdbs_work`, `a_revert_pays_for_the_golemdb_work_it_undid`, `golemdbs_work_can_run_a_batch_out_of_gas`; `op_costs_come_from_the_store_or_the_placeholder` (`gas.rs`) | `a_create_buys_its_size_until_expiry`, `a_patch_buys_only_its_growth`, `a_patch_that_grows_and_shrinks_buys_its_net_growth`, `an_extend_buys_only_the_added_blocks`, `storage_gas_is_rounded_once_per_batch` |
| Settlement (`crates/arkiv-reth-executor/src/lib.rs`) | `golemdbs_work_reaches_settlement`, `a_reverted_transaction_pays_its_golemdb_work` | `the_pool_gets_the_storage_share_and_the_value` |
| Running node (`bin/arkiv-reth/tests/e2e.rs`) | `ops_that_leave_the_attributes_alone_cost_the_same_however_large_the_entity` (relative, on GolemDB) | `the_pool_receives_the_storage_share_and_the_value`, `a_create_a_patch_and_an_extend_show_their_compute_and_storage_gas`, `estimates_are_isolated_and_a_shrinking_patch_buys_no_storage`, `storage_that_does_not_fit_runs_a_create_out_of_gas` |

Not covered yet: an e2e test that pins the absolute compute gas of an op on GolemDB against
the genesis weights. The SDK script measures it, but asserts nothing.

## 4. Size

+13,248 / −623 lines against `main`'s merge base `5db9e58`. Test modules inside `src/`
count as tests; so do `arkiv-harness`, the store and statemanager conformance suites and
the test store that charges per call.

| | Added | Removed | Share of added |
| --- | ---: | ---: | ---: |
| Production code | 2,051 | 424 | 15% |
| Tests | 7,833 | 163 | 59% |
| Docs | 2,547 | 17 | 19% |
| Experiment and dev container (`kurtosis/integration/experiment`, bun) | 626 | 3 | 5% |
| Config (Cargo, genesis files) | 191 | 16 | 1% |

By group:

| Group | Production | Tests | Docs | Other |
| --- | ---: | ---: | ---: | ---: |
| Iteration 1, as first reviewed (`5ea8641`) | +2,104 / −374 | +7,261 / −122 | +2,230 / −3 | +85 / −1 |
| GolemDB pricing (#250) | +381 / −322 | +540 / −513 | +109 / −69 | +133 / −42 |
| Follow-ups | +208 / −21 | +167 / −12 | +4 / −2 | +34 / −34 |
| SDK script and dev container | – | – | +296 / −35 | +701 / −78 |

Production code by crate:

| Crate | Added | Removed |
| --- | ---: | ---: |
| `arkiv-interfaces` | 584 | 40 |
| `arkiv-tokenomics` (new) | 500 | 0 |
| `arkiv-reth-executor` | 341 | 221 |
| `arkiv-golemdb-state` | 209 | 58 |
| `arkiv-golemdb`, `arkiv-valkey` | 273 | 78 |
| others | 144 | 27 |

Docs: the iteration plan (803 lines, the decision log), `tokenomics-spec.md` (342),
`sdk-next-testing.md` (329), `tokenomics-execution.md` (291), `purging-spec.md` (191), this
guide, `e2e-load-test.md`, and additions to `api-spec.md`, `genesis-spec.md` and the README.

The largest test files: the unit tests in `arkiv.rs` (2,067 lines in the file), the load
test (`run.rs` 909, `plan.rs` 523), the tests in `lib.rs` (767), `e2e.rs` (698 added), the
store conformance suite (441) and `metering_tests.rs` (397).
