# Arkiv tokenomics V1: what is implemented

State of `main` at v1.0.0 (commit `827a83b`), 2026-11-01. This document describes the fee model
as the code implements it. It replaces `docs/tokenomics-iteration-1-plan.md` on the branch
`matthiaszimmermann/feat/tokenomics-init` as the reference for what exists; that plan is partly out
of date. Where this document and the code disagree, the code is right and this document has a bug.

## Management summary

- **One fee, as on Ethereum.** Every Arkiv transaction pays `gas used × gas price`, with the same
  EIP-1559 base fee and tip that Ethereum uses. Wallets and the SDK need nothing special.
- **Gas has two parts.** *Compute gas* pays for the work a transaction does: its calldata, and the
  database work GolemDB measures for it. *Storage gas* pays for the space an entity occupies,
  multiplied by the number of blocks it occupies it. Storing more, or for longer, costs
  proportionally more.
- **All prices come from the genesis file.** 32 GolemDB weights and 6 Arkiv storage parameters set
  every price. They are fixed for the lifetime of a chain: changing them means a new chain.
- **Today's values are placeholders, not calibrated prices.** On the dev chain, the floor that
  Ethereum puts on calldata (EIP-7623) currently dominates the compute fee, and the GolemDB weights
  barely register. The storage price works out at about $1.80 per GB per month at a 1 gwei base
  fee and GLM at $0.10.
- **Throughput is bounded by the block gas limit.** It is effectively fixed at 30M gas per block
  because of a bug: the configured target is ignored. On rabdi, about 250 small entities fit in
  one block. How much reserve the nodes have beyond that has not been measured.
- **Storage fees go to a pool, not into the burn.** The base-fee share of compute gas is burned,
  as on Ethereum. The base-fee share of storage gas is credited to a pool account at the Arkiv
  system address. Tips go to the block producer.
- **Next steps:** fix the gas-limit bug, measure how long full blocks take the nodes against the
  2-second slot, calibrate the GolemDB weights against measured time, and set the storage
  parameters from economic targets. Each round of changes needs a new test chain.

## Contents

1. [How fees work in one page](#1-how-fees-work-in-one-page)
2. [Fee calculation](#2-fee-calculation)
   - [2.1 Gas used](#21-gas-used)
   - [2.2 Compute gas](#22-compute-gas)
   - [2.3 GolemDB cost](#23-golemdb-cost)
   - [2.4 Storage units](#24-storage-units)
   - [2.5 Storage time](#25-storage-time)
   - [2.6 Storage gas](#26-storage-gas)
   - [2.7 Payment and where it goes](#27-payment-and-where-it-goes)
   - [2.8 Reverts, running out of gas, bad calldata](#28-reverts-running-out-of-gas-bad-calldata)
   - [2.9 Rules that are not prices](#29-rules-that-are-not-prices)
   - [2.10 Expired entities and the purge](#210-expired-entities-and-the-purge)
   - [2.11 A worked example](#211-a-worked-example)
3. [Getting to calibrated parameters](#3-getting-to-calibrated-parameters)
4. [Where it is in the code](#4-where-it-is-in-the-code)

## 1. How fees work in one page

Arkiv is a reth-based chain without an EVM. Transactions to the Arkiv system address
`0x4400000000000000000000000000000000000044` carry a batch of entity operations: create, patch,
extend, transfer and delete. Any other transaction is a plain value transfer. Contract creation is
refused.

An entity transaction is metered as it runs:

1. The transaction's own terms are priced from its calldata, as on Ethereum.
2. Each operation calls GolemDB, Arkiv's database. GolemDB returns the cost of each call, computed
   from its genesis weights. These costs add up to the GolemDB cost.
3. Each create, extend or growing patch buys *storage time*: storage units multiplied by blocks.
4. The sender pays `gasUsed × effective gas price`. The base-fee share of storage gas goes to the
   pool, the rest of the base fee is burned, and the tip goes to the block producer.

A transaction never pays more than its gas limit. If it would, it runs out of gas and pays the
whole limit.

## 2. Fee calculation

Notation: all gas values are whole numbers. `⌈x⌉` rounds up.
Every sum and product is checked: an overflow does not wrap or saturate; it ends the transaction
as out of gas, identically on every node.

### 2.1 Gas used

```
gasUsed = compute_gas + storage_gas + purge_gas

purge_gas = 0                                    (not collected yet; see 2.10)
```

`gasUsed` is what the receipt shows and what the sender is charged for.

### 2.2 Compute gas

```
compute_gas = max( tx_gas + golemdb_cost , floor_gas )

tx_gas    = 21,000 + 4  × tokens(calldata)
floor_gas = 21,000 + 10 × tokens(calldata)

tokens(calldata) = zero_bytes + 4 × nonzero_bytes
```

- `tx_gas` is Ethereum's intrinsic gas: 4 gas per zero byte and 16 per non-zero byte of calldata
  (EIP-2028).
- `floor_gas` is Ethereum's calldata floor (EIP-7623, Prague): 10 gas per token. It is applied
  once per transaction, to compute gas only.
- `golemdb_cost` is the sum of the costs of every GolemDB call the transaction makes (2.3). That
  includes reading the sender's entity nonce at the start, from which new entity keys are derived.
- Arkiv adds no per-operation charge of its own on GolemDB. (A provisional per-operation price list,
  `PlaceholderCost`, exists only for the in-memory test store, which reports no costs. It is never
  used on a GolemDB node.)

A plain value transfer has no GolemDB cost and buys no storage:

```
gasUsed(transfer) = max( 21,000 , tx_gas , floor_gas )
```

### 2.3 GolemDB cost

GolemDB prices each call from a *ledger*: how many times each kind of work was done. The call's
cost is the ledger weighed with the genesis weights:

```
call_cost = Σ_w  count_w × weight_w        over the 32 GolemDB weights w
```

For a write, GolemDB's own specification (`docs/golemdb-metering.md` in the GolemDB repository)
defines the counts:

```
record op cost       = admission cost + read.key + write.record.{create | patch | delete}
                     + Σ cell operation costs
                     + Σ index operation costs

admission cost       = write.admission.base
                     + entries inspected × write.admission.entry
                     + bytes validated   × write.admission.byte

cell operation cost  = read.cell                                  (only when read)
                     + write.cell.{create | update | delete}
                     + cell_trie_depth × write.cell_trie_update
                     + cell bytes written × write.cell_byte

index operation cost = read.term + write.index.{join | leave}
                     + index_trie_depth × write.index_trie_update
                     + write.index_term_create                    (only if the term is new)
                     + index bytes written × write.index_byte
```

- Trie depth is modelled, not measured: `⌈log₁₆ N⌉` of the trie's size at the start of the
  block, capped at 7. A busier database therefore charges slightly more per write.
- Work is charged even if it is later rolled back. Nothing is refunded.
- The weights for standalone reads, queries and logs are declared, and must be set in the genesis
  file, but GolemDB does not meter that work during execution yet. Queries over RPC are not part
  of consensus and are not charged.

### 2.4 Storage units

The size of an entity, weighed with four genesis parameters:

```
storage_units = cell_bytes    × cell_byte_units
              + index_bytes   × index_byte_units
              + cells         × c_cell
              + indexed_cells × c_idx
```

The four counts come from GolemDB's per-record metadata (`#meta`), never from Arkiv's own
counting. They cover the entity's user attributes; GolemDB's fixed-size system cells are not
included. GolemDB counts bytes this way:

- a cell: `8 + |name|` bytes of key, plus `1 + |value|` bytes of value
- an index entry: `|name| + 2 + |value|` bytes

For a write, the change in units is computed the same way from the write's details: everything
it wrote, minus everything it deleted. An updated cell counts its old bytes out and its new bytes
in.

```
units_delta = units(written) − units(deleted)            (may be negative)
growth      = max(units_delta, 0)
```

### 2.5 Storage time

Each operation buys storage time, in unit-blocks:

| Operation | Storage time bought |
| --- | --- |
| create | `growth × (expires_at − current_block)`. A create's growth is the entity's whole size. |
| patch | `growth × (expires_at − current_block)`: only net growth, for the rest of the entity's life. A patch that shrinks the entity, or keeps its size, buys nothing and refunds nothing. |
| extend | `units_before × (new_expires_at − old_expires_at)`: the size the entity has, for the blocks added. |
| transfer | 0 |
| delete | 0. No refund for the time not used. |

`current_block` is the number of the block the transaction is included in. The storage time of a
transaction is the sum over its operations:

```
storage_time = Σ_ops storage time bought
```

### 2.6 Storage gas

```
storage_gas = ⌈ storage_time × capacity_rate / 1,000,000 ⌉
```

`capacity_rate` is in micro-gas per storage unit per block. The rounding is applied once per
transaction, so a batch never pays more than the same storage bought in one operation.

### 2.7 Payment and where it goes

```
effective_price = min( max_fee_per_gas , base_fee + max_priority_fee_per_gas )
                  (for a legacy transaction: its gas_price)

fee paid by sender = gasUsed × effective_price

burned             = compute_gas × base_fee
to the pool        = storage_gas × base_fee           (+ any value sent to the Arkiv address)
to block producer  = gasUsed × (effective_price − base_fee)
```

- The sender is charged for gas used, not for the gas limit, and nothing is reserved up front.
  Before execution, the sender must still be able to afford `gas_limit × max_fee_per_gas + value`,
  as on Ethereum.
- The pool is the balance of the Arkiv system address. Nothing pays out of it yet.
- The base fee follows EIP-1559: it targets half the block gas limit and changes by at most 1/8 per
  block. Arkiv adds one rule: it never falls below the genesis `baseFeePerGas`. Without that value
  in the genesis file, the floor is reth's 1 gwei default.

### 2.8 Reverts, running out of gas, bad calldata

| Outcome | Gas charged | Storage bought | Entity changes |
| --- | --- | --- | --- |
| Success | `gasUsed` as above | yes | kept |
| Business-rule revert (e.g. not the owner, entity not found, lifetime too long) | `min( max(tx_gas + golemdb_cost_so_far, floor_gas), gas_limit )` | none | none |
| Out of gas, or an overflow | the whole `gas_limit` | none | none |
| Calldata that does not decode | `min( max(tx_gas + cost of the nonce read, floor_gas), gas_limit )` | none | none |

Before each operation runs, the executor checks that the compute so far fits the gas limit. After
it runs, it checks that compute, storage and the floor together still fit. If either check fails,
the transaction is out of gas.

A transaction whose gas limit is below `max(tx_gas, floor_gas)` is invalid: it is not included at
all.

### 2.9 Rules that are not prices

These affect what may be bought, not what it costs:

- **Maximum lifetime.** A create or extend may not set an expiry more than `max_lifetime` blocks
  after the current block. The rule bounds the lifetime that *remains*, so an entity can be
  extended again and again, paying as it goes. An extend that does not move the expiry is
  allowed, and checks nothing. Expiries never shorten. A permanent expiry is always refused.
  Breaking the rule is a business-rule revert (`ExpiryBeyondMaxLifetime`).
- **Size limits** from the genesis `cell_limits` and `record_limits`: the length of a name,
  a string and a bytes value, and the number of cells and indexed cells per entity.

### 2.10 Expired entities and the purge

Expired entities are removed by a protocol transaction that the block producer adds at the start
of every block. Nobody pays for it:

- it removes at most 10 expired entities (`MAX_PURGE_KEYS`), within a gas limit of 1,000,000.
  If more expire at once, the rest wait for later blocks.
- each removal counts `10,000 + 5,000 × indexed attributes` gas towards that limit. These are
  provisional numbers, not GolemDB costs.
- its gas counts towards the block's `gasUsed`, so it takes up to 1M gas of block space
- it pays no fee, so the purge work is currently funded by nobody

`purge_gas`, a deletion reserve paid at creation time, is part of the formula but is always 0 in
this version.

### 2.11 A worked example

From the tokenomics crate's tests, with `capacity_rate = 14`: an entity of 1,556 storage units
created to expire 3,888,000 blocks (90 days at 2 s per block) after the current block.

```
storage_time = 1,556 × 3,888,000           = 6,049,728,000 unit-blocks
storage_gas  = ⌈6,049,728,000 × 14 / 10⁶⌉   = ⌈84,696.192⌉ = 84,697 gas
```

At a base fee of 1 gwei, the pool receives 84,697 gwei, about 0.0000847 GLM.

What this price means per byte (taking one storage unit as one byte, and 30 days as 1,296,000
blocks at 2 s each):

```
14 × 1,296,000 / 10⁶ = 18.144 gas per unit per 30 days
1 GB for 30 days     = 18.144 × 10⁹ gas = 18.144 GLM at a 1 gwei base fee ≈ $1.81 at $0.10/GLM
```

## 3. Getting to calibrated parameters

The two kinds of parameters answer different questions and should be tuned separately:

- **Engine weights** answer *how much work is this?* They should be proportional to the real time
  and I/O a node spends, so that a full block of any mix of operations takes about the same time
  to process. They are tuned by measurement.
- **Storage parameters** (`capacity_rate`, the unit weights, `max_lifetime`) answer *what should
  holding data cost?* They are tuned from economic targets: a price per GB-month, an acceptable
  database growth per year, the cost of the largest entity a user should be able to buy in one
  transaction.

A sequence that builds on what exists:

1. **Fix the block gas limit bug**, so a target other than 30M can be set and tested.
2. **Measure time on a copy of the chain.** On a local node or Kurtosis with rabdi's genesis, fill
   blocks and record each block's build time on the sequencer, validation time on the followers,
   and GolemDB commit time, at the slow end (p95/p99) rather than on average. Do it with a database
   pre-filled to several months of expected state, not an empty one.
3. **Calibrate the engine weights.** GolemDB logs each call's ledger with
   `--log.stdout.filter 'arkiv::golemdb::metering=debug'`. Fit measured time against the ledger
   counts across mixed workloads, and set each weight proportional to its time. Then decide
   whether the EIP-7623 calldata floor should stay the dominant term.
4. **Raise the block gas limit step by step**, for example 30M → 45M → 60M, and keep a full block
   under about a third of the 2-second slot. That leaves room for propagation, the consensus client
   and slower follower hardware.
5. **Set the storage parameters** from the economic targets, and check that the largest entity
   meant to be buyable for the full lifetime fits the per-transaction gas limit.
6. **Repeat on a fresh test chain** for each set of values. Pricing is fixed at genesis: there is
   no way to change it on a running chain yet.

Things to settle along the way:

- the purge is unpaid, and `purge_gas` is always 0
- at most 10 expired entities are removed per block, so a sustained rate above 5 expiries per
  second builds a backlog
- nothing pays out of the storage pool yet
- standalone reads, queries and logs are not metered in execution
- the purge's per-entity cost is a provisional constant, not a GolemDB cost

## 4. Where it is in the code

| What | Where |
| --- | --- |
| Gas meter: compute, floor, storage rounding | `crates/arkiv-tokenomics/src/meter.rs` |
| Storage units | `crates/arkiv-tokenomics/src/units.rs` |
| Storage time per operation, lifetime rule | `crates/arkiv-tokenomics/src/storage_time.rs` |
| Host parameters, read from genesis | `crates/arkiv-tokenomics/src/schedule.rs` |
| Batch execution and metering per operation | `crates/arkiv-reth-executor/src/arkiv.rs` (`run_batch`, `stage_op`) |
| Calldata gas, settlement, pool credit | `crates/arkiv-reth-executor/src/lib.rs` (`tx_gas`, `floor_gas`, `charge_sender`, `credit_pool`) |
| Per-block pricing snapshot | `crates/arkiv-reth-executor/src/lib.rs` (`BlockPricing`) |
| Base-fee floor | `crates/arkiv-reth-chainspec/src/lib.rs` (`next_block_base_fee`) |
| Purge transaction | `crates/arkiv-reth-payload-builder/src/lib.rs` (`protocol_transaction`) |
| Provisional costs (purge; test store only) | `crates/arkiv-interfaces/src/gas.rs` |
| GolemDB weights and their cost formulas | GolemDB repository: `crates/metering`, `docs/golemdb-metering.md` |
| Genesis format | `docs/genesis-spec.md` |
