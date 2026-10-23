# Tokenomics iteration 1: implementation plan

Status: draft, 2026-10-23. Spec: `docs/tokenomics-spec.md`. GolemDB context:
`golemdb-metering.md` and `write-metering-implementation-plan.md` in the
GolemDB repo.

## Goal

Senders pay for the bytes they store, times the blocks they keep them, and
that payment goes into a storage pool instead of being burned. Everything else
in the tokenomics spec stays as it is today.

Concretely, iteration 1:

1. reads the fee schedule from store cells (values faked for now),
2. passes GolemDB's metered results (receipts, budgets) through to Arkiv,
3. reads each entity's `#meta` counts,
4. prices storage × lifetime and credits it to the pool.

## Baseline

`main` at `9e678a2`, after PR #222 (golemdb/query_integration):

- GolemDB is pinned at `d910d14`: its calls return `Metered<T>` and take
  explicit budgets, records carry `#meta`, its genesis type has record limits,
  and `golemdb-metering` (#91) brings `Schedule` with `host.*` weights,
  `WriteMeter` and `Receipt.ledger`.
- The Arkiv adapter passes budgets to GolemDB's record calls, but drops every
  receipt (`metered_result` in `crates/arkiv-golemdb/src/convert.rs`), and
  Arkiv's `Record` does not carry `#meta`.
- GolemDB still charges cost 0 on every call: its write pricing lands with its
  own iterations 4–6.

## Scope

| In | Out (later iterations) |
| --- | --- |
| (GolemDB at `d910d14`: done by #222) | real GolemDB compute costs (still 0) |
| receipts and budgets passed through Arkiv's store layer | purge reserve (D5 bound, `purge_price`) |
| `#meta` counts read for entities | tip on compute gas only |
| fee schedule as store cells, seeded from the genesis | compute-only block `gasUsed` and base fee |
| storage units, storage gas for create, extend, growing patch | storage priced independently of the base fee (the spec's `storage_price`) |
| `$rate` per entity | receipt fields `computeGas` / `storageGas` / `purgeGas` |
| storage share credited to a pool account | estimation headroom `k`, `arkiv_estimateFee`, `arkiv_getFeeSchedule` |
| | pool payouts to validators, expiry ledger, `R` / `L`; pool address in the genesis |
| | limits: `max_lifetime`, `max_block_storage_units`, `max_live_bytes` |

Compute gas stays `PlaceholderCost`. All fee-schedule values are placeholders.

## Architecture

### New crate: `arkiv-tokenomics`

Pure, deterministic pricing, depending only on `arkiv-interfaces`: no reth, no
store. It holds the consensus-critical arithmetic in one reviewable place,
like `entity_records` does for the record layout, and is reusable by the RPC
and CLI later.

- `FeeSchedule`: the `host.arkiv.*` values iteration 1 needs.
- Storage units from `#meta` counts and the schedule.
- Unit-blocks per operation; storage fee in wei; conversion to gas.
- `$rate` on create, extend and patch.

I/O (reading the schedule, writing `$rate`, crediting the pool) stays in the
state and executor layers. `PlaceholderCost` (`arkiv-interfaces/src/gas.rs`)
can move into the crate in a later iteration.

### Plug-in points

| Concern | Where |
| --- | --- |
| GolemDB API and receipts | `crates/arkiv-golemdb` (adapter), `crates/arkiv-interfaces/src/store.rs` (`Receipt`, `Record`) |
| Entity writes and their receipts | `crates/arkiv-golemdb-state/src/entities.rs` (`store.create` / `patch` / `delete`, receipts dropped today) |
| Genesis values | `crates/arkiv-reth-chainspec/src/lib.rs` (`ArkivConfig`, today only `golemdb`) |
| Genesis seeding (covered by the genesis root) | `crates/arkiv-reth-statemanager/src/genesis.rs` (`seed_genesis`), `bin/arkiv-reth/src/genesis.rs` |
| Gas per operation, staging | `crates/arkiv-reth-executor/src/arkiv.rs` (`apply_with_effects`) |
| Transaction gas, settlement | `crates/arkiv-reth-executor/src/lib.rs` (`arkiv_entity_transact`, `charge_sender`, `FeeEnv`) |
| Entity cells | `crates/arkiv-interfaces/src/entity.rs` (`annotations`), `crates/arkiv-interfaces/src/entity_records.rs` |

## Pricing rules (iteration 1)

From `docs/tokenomics-spec.md`, restricted to storage, with one simplification:
**storage is priced in gas, folded into the single gas fee.** The spec prices
storage in wei at a fixed `storage_price`, independent of the base fee, and
converts it to gas by dividing by the base fee; that separate pricing is a
later iteration. In iteration 1 storage gas is paid at the effective gas
price like compute gas, so its price moves with the base fee.

```
storage_units = cell_bytes × cell_byte_units + index_bytes × index_byte_units
              + cells × c_cell + indexed_cells × c_idx          (all counts from #meta)

unit_blocks   = create:         storage_units × lifetime
                extend:         storage_units × added_blocks
                growing patch:  Δ storage_units × remaining_lifetime
                otherwise:      0                                (no refunds)

storage_gas   = ceil(unit_blocks × storage_gas_rate / 1_000_000)  (checked u128)
gas_used      = compute gas (PlaceholderCost, as today) + Σ storage_gas
storage_fee   = base_fee × storage_gas                            (wei: the pool's share)
```

`storage_gas_rate` is in micro-gas per unit-block: a unit-block is tiny (the
spec's 1 KiB entity for 3 months is about 6 × 10⁹ unit-blocks), so a whole-gas
rate would be far too coarse.

`$rate` is the wei per block an entity's storage was bought at, as in the
spec's storage-pool table: `storage_fee / lifetime` on create;
`(rate × remaining + added_fee) / new_remaining`, rounded down, on extend and
growing patch; scaled by `new_units / old_units` on a shrinking patch; removed
on delete.

The fee schedule uses `host.arkiv.*` names, as GolemDB's `Schedule` will carry
once it persists host weights: `host.arkiv.storage_gas_rate`,
`host.arkiv.cell_byte_units`, `host.arkiv.index_byte_units`,
`host.arkiv.c_cell`, `host.arkiv.c_idx`.

## Steps

Each step leaves the full CI pipeline green (fmt, clippy, nextest, Valkey
tests) and is its own commit.

1. **GolemDB bump to `d910d14`.** Done by PR #222: the pin, the adapter
   migration, budgets on record calls, `record_limits` in `genesis/*.json`.

2. **Receipts and budgets through Arkiv.**
   Extend Arkiv's `Receipt` with GolemDB's write details and ledger; the
   adapter maps them instead of dropping them, and passes Arkiv's
   `Option<Budget>` on to GolemDB (unlimited where none is given). MemStore
   and Valkey report zero receipts. `arkiv-golemdb-state` collects the
   receipts of a transaction's writes and hands them to the executor.
   *Done when:* a test sees one receipt per store write, with byte details;
   cost is still 0.

3. **`#meta` into Arkiv.**
   `Record` gains `meta` (cells, cell bytes, indexed cells, index bytes),
   parsed from GolemDB's `#meta`; MemStore and Valkey compute the same counts
   from their cells on GolemDB's size rule (cell `(8+name)+(1+value)`, index
   entry `name+2+value`). An entity-level accessor in `arkiv-golemdb-state`.
   *Done when:* for created, patched and deleted entities, the counts match on
   all three stores and equal the size rule applied to
   `entity_records::to_cells`.

4. **`arkiv-tokenomics` crate.**
   `FeeSchedule`, storage units, unit-blocks per operation, fee and gas
   conversion, `$rate`.
   *Done when:* unit tests reproduce the spec's worked example (create, patch,
   extend: storage gas and `$rate`).

5. **Fee schedule as cells.**
   `config.arkiv.tokenomics` in the chain spec; seeded at genesis into a
   fee-schedule record with one cell per `host.arkiv.*` name; the executor
   reads it from the block's base state behind a small `FeeScheduleSource`
   trait, so GolemDB's pricing snapshot can replace it later. Placeholder
   values in `genesis/dev.json` and `genesis/example.json`.
   *Done when:* the genesis hash changes with the values, and the executor
   prices with the configured `storage_gas_rate`.

6. **Charging and the pool.**
   - `ExecEnv` gains `base_fee` (from `FeeEnv`), for the pool's share and
     `$rate`.
   - The executor writes the batch first, under a view checkpoint, then reads
     the touched entities' `#meta`, computes storage gas for create, extend and
     growing patch, and writes `$rate`. If compute plus storage gas exceeds the
     gas limit, it rolls back to the checkpoint and reverts with `OutOfGas`
     (`GolemStateView` already offers checkpoint and rollback). A temporary
     solution: the spec's pre-check computes a create's storage gas from its
     payload before execution.
   - The pool is an **account**, with nonce and balance like any other, at a
     constant address (`ARKIV_STORAGE_POOL_ADDRESS` in
     `arkiv-interfaces/src/constants.rs`, beside `ARKIV_ADDRESS`).
     `charge_sender` credits it `base_fee × storage_gas`; the rest of the base
     fee is burned and the tip is paid as today. Its balance is visible through
     `eth_getBalance`.
   *Done when:* e2e tests show a create's `gasUsed` including the storage
   part, the pool account's balance rising by exactly `base_fee × storage_gas`,
   an extend charging only the added blocks, a shrinking patch charging and
   refunding nothing, and a batch whose storage gas does not fit reverting with
   `OutOfGas` and leaving no entity behind.

7. **Integration check.**
   Run the SDK's network-health suite against a local node on GolemDB.
   `eth_estimateGas` includes storage gas automatically, since it runs the
   same executor. Record what changes for clients (gas figures).

Steps 4 and 5 do not depend on GolemDB and can run in parallel with 2–3.

## Decisions

Decided (2026-10-23):

1. **Pool location:** an account, with nonce and balance, at a constant
   address for now. The address moves into the Arkiv genesis later.
2. **Storage units:** write first, read `#meta`, and roll back if out of gas.
   Temporary; a pre-execution estimate (the spec's pre-check) should replace
   it eventually.
3. **Gas size:** storage and compute fold into a single gas fee in this
   iteration; storage priced independently of the base fee (the spec's
   `storage_price`) is a later iteration.

Still open:

4. **Fee-schedule record key** and `$rate`'s spelling: a fixed, documented
   record key; `$rate` as a field (not queryable), added to `annotations` and
   to the API spec's system attributes and reserved-name rules.
5. **Placeholder values,** chiefly `storage_gas_rate`: storage gas must fit the
   dev chain's 30M block gas limit for realistic entities and lifetimes.

## Coordination

- The adapter merged with PR #222 drops receipts on purpose; step 2 passes
  them through. Worth a heads-up to its author.
- GolemDB's write pricing (its iterations 4–6) will replace cost 0 with real
  costs, and its iteration 5 will persist `host.*` weights at genesis. Step
  5's `FeeScheduleSource` is the seam for that switch.
