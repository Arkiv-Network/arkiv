# Arkiv Genesis

What an Arkiv genesis file contains, how a node turns it into block 0, and the
remaining implementation plan. Pricing is implemented as described below;
the full record-oriented layout remains a draft. Sections marked *planned* depend on work that
has not landed yet (see [Implementation plan](#implementation-plan)).

Background: [GolemDB design §4](https://github.com/Arkiv-Network/golemdb/blob/main/docs/golemdb-design.md#4-system-admin-and-user-records)
(superblock; system, admin and user records), GolemDB metering D9 (cost
schedules), `arkiv-tokenomics.md` in the GolemDB repo (fee schedule, `arkiv_state`).

## 1. Target principles

1. **A standard genesis, plus one `arkiv` object.** Everything Ethereum tooling
   understands (`config`, `alloc`, `baseFeePerGas`, …) keeps its meaning. All
   Arkiv-specific content lives under one `arkiv` key. The implementation uses
   `config.arkiv`; the target format below proposes a top-level key.
2. **Sections are named after what they are.** Under `arkiv.golemdb`, the
   committed records keep their own names (`#params`, `@meteringModel`,
   `@modelWeight`) and cell names; the consensus encoding rules are grouped as
   `encoding`, named for their role, not for where GolemDB stores them.
3. **Record IDs belong to GolemDB.** Neither the file nor Arkiv names a record
   ID. GolemDB's genesis maps every value to its record: system (0–31) and admin
   (32–63) records have fixed IDs, and user records get IDs from GolemDB's
   `#nextRecordID` counter in the order they are created.
4. **Everything in the file is committed.** The genesis header's `stateRoot` is
   GolemDB's global root, so two files that differ anywhere produce different
   genesis hashes: a mismatch is a root mismatch, never a silent divergence.
5. **Typed sections only.** No generic "records" or "rows" escape hatch: every
   section has a fixed shape the node validates.

## 2. Implemented pricing configuration

The runnable files are [`dev.json`](../genesis/dev.json) and
[`example.json`](../genesis/example.json). They use GolemDB's native genesis
format under `config.arkiv.golemdb`, with a required `metering` object:

```json
{
  "golemdb_code_version": 1,
  "host_code_version": 1,
  "weights": {
    "write.record.create": 0,
    "host.arkiv.capacity_rate": 14,
    "host.arkiv.cell_byte_units": 1,
    "host.arkiv.index_byte_units": 1,
    "host.arkiv.c_cell": 64,
    "host.arkiv.c_idx": 64,
    "host.arkiv.max_lifetime": 7884000
  }
}
```

This excerpt omits other engine weights: both shipped files explicitly list all
32 GolemDB weights and all six host weights. Engine prices are all 1 as an
initial nonzero baseline; host values preserve existing storage charges.
These prices are not calibrated for production. Missing engine weights remain
unpriced; missing or zero host values fail startup. Neither omission implies zero.

To inspect engine charges, add
`--log.stdout.filter 'arkiv::golemdb::metering=debug'` when starting the node.
Create, patch/update and delete calls log their operation, cost, pricing commit,
per-weight ledger counts, write details and error, including failed writes.
Get, query and count calls do not emit these logs. These are individual engine
calls, including simulation and genesis work, rather than transaction fee summaries.

GolemDB writes model 1, activated at commit 0, into `@meteringModel` and
`@modelWeight`. Arkiv supplies host code version 1 and its six supported weight
names on both disk and memory opening paths. JSON parsing rejects duplicate
weights, unknown names, unsupported code versions and invalid host values.

`Store::pricing_snapshot(commit)` reads `metering_model` and `model_weights`
for the same explicit commit and active version. `FeeSchedule` is a checked,
typed view of the six host weights. Each block execution captures it with the
pricing commit and model version; transactions and simulation probes reuse that
snapshot. An unavailable or stale base fails rather than selecting head prices.
The current GolemDB API supports inspection at the head only, matching Arkiv's
current writable branch model. Historical execution and runtime model activation
still require upstream support; this change does not implement either.

There is no `config.arkiv.tokenomics`, separate fee-schedule user record, or
production placeholder constructor. `seed_genesis` writes accounts only; the
engine has already committed pricing. Changing a price changes the genesis
hash, and reopening a database with changed genesis pricing fails. These are
breaking genesis changes: development chains need fresh databases, with no
migration of existing chain state provided.

Experimental MemStore and Valkey receive the genesis model explicitly in memory.
They do not persist or update pricing models and are not consensus backends.
Unconfigured stores fail pricing inspection; test fixtures must supply values.

## 3. Target file format (partly planned)

```json
{
  "config": { "chainId": 424242, "londonBlock": 0, "terminalTotalDifficulty": 0,
              "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0 },
  "baseFeePerGas": "0x3b9aca00",
  "gasLimit": "0x3938700",
  "difficulty": "0x0",
  "alloc": {
    "0x1111111111111111111111111111111111111111": { "balance": "0x3635c9adc5dea00000" }
  },
  "arkiv": {
    "golemdb": {
      "encoding": { "hash_fn": "keccak-256", "roaring": 1 },
      "#params": {
        "#maxCellNameLen": 64,
        "#maxStrLen": 128,
        "#maxBytesLen": 131072,
        "#keyMode": 0,
        "#maxRecordCells": 64,
        "#maxRecordIndexedCells": 32,
        "#minActivationDelay": 43200,
        "#minRetention": 128
      },
      "@meteringModel": { "golemdbCode": 1, "hostCode": 1 },
      "@modelWeight": {
        "…golemdb weights…": 0,
        "host.arkiv.capacity_rate": 14,
        "host.arkiv.cell_byte_units": 1,
        "host.arkiv.index_byte_units": 1,
        "host.arkiv.c_cell": 64,
        "host.arkiv.c_idx": 64,
        "host.arkiv.max_lifetime": 7776000
      }
    },
    "arkiv_state": { "balance": "0x0" }
  }
}
```

Values are placeholders.

| Field | Meaning | Becomes | Status |
| --- | --- | --- | --- |
| `config`, `gasLimit`, `difficulty`, … | Standard, chain config | reth chain spec | done |
| `baseFeePerGas` | Standard, genesis base fee, and the chain's `min_base_fee` floor | header; base-fee rule | done |
| `alloc` | Standard, prefunded accounts: `balance`, optional `nonce` | one user record per address (`$balance`, `$nonce`) | done on MemStore/Valkey |
| `golemdb.encoding` | Custom, consensus encoding rules: hash function and Roaring serialization version; both determine the roots | stored by GolemDB (superblock `hash_fn`, `roaring`) | planned (phase 1) |
| `golemdb.#params` | Custom, chain parameters, immutable after genesis | `#params` (record 0) | cell limits: phase 1; the rest: phase 3 |
| `golemdb.@meteringModel` | Custom, GolemDB and host (Arkiv) code versions of model 1; activation is always commit 0 | `@meteringModel` (record 32) | pricing implemented via `golemdb.metering`; target JSON layout planned |
| `golemdb.@modelWeight` | Custom, model 1's weights: GolemDB's own and Arkiv's `host.*` prices and limits | `@modelWeight` (record 33) | pricing implemented via `golemdb.metering`; target JSON layout planned |
| `arkiv_state` | Custom, seed of Arkiv's protocol state: the storage pool `balance`; `R`, `L`, `purge_cursor`, `live_bytes` start at 0 | the `arkiv_state` user record | planned (phase 4) |

## 4. Rules

- **GolemDB writes its own records.** GolemDB's genesis creates every reserved
  record and fills it from `arkiv.golemdb`. The node hands it the values and the
  ordered list of user records; it never writes a reserved record itself.
- **Encoding rules are chain values.** `hash_fn` and `roaring` decide the bytes
  that are hashed, so they are fixed by the chain, not by the binary: a node
  refuses values outside the set it supports (G3) rather than substituting its
  own. Changing either on a live chain is a hard fork with an activation height.
- **Segments come from the node, not the file.** `#shardSpan` and
  `#immutableDataSegments` are fixed by the code that writes the segments (the
  node already appends its own `commit_tags` segment). The node adds them to
  `#params`; the file cannot set them.
- **Fixed user-record order.** User record IDs follow creation order, and the
  state trie is keyed by record ID, so order determines the root. The node passes
  `arkiv_state` (when implemented), then accounts by ascending address. Nothing iterates a
  hash map.
- **Key mode is caller-assigned** (`#keyMode` 0). Accounts (padded address) and
  `arkiv_state` have keys Arkiv chooses.
- **Header commits to the store.** The genesis header's `stateRoot` is GolemDB's
  global root, `Hash(0x06 ‖ StateRoot ‖ IndexRoot)`, after the genesis branch is
  sealed. The genesis hash is computed from that header, and only then is the
  commit tagged with it (tags live in a segment, outside the root, so there is
  no cycle).
- **Genesis is one atomic commit.** GolemDB's genesis applies the settings and
  the user records together as commit 0, so a store is either empty or fully
  initialized; there is no state with engine records but no accounts.
- **Opening always checks the genesis.** The store has one `open`, and the
  genesis is a required argument: an empty store is created from it; an
  existing store is opened only if its genesis commit carries the same genesis
  hash, which covers every field (principle 4). Nothing opens a store without
  that check; should a tool ever need to, it gets a separately named
  constructor, not an optional argument.
- **Versions must match.** See G3.

## 5. Deliberately not in the file

| Not included | Why |
| --- | --- |
| Superblock `format` | Local on-disk layout, not consensus: nodes with different layouts reach the same roots. Owned by the binary, which migrates or refuses a database it cannot open (G3) |
| `#alloc`, `#roots`, `#recordKeys`, `#rootIndex` | Engine bookkeeping, maintained by GolemDB |
| Record IDs | Assigned by GolemDB (principle 3) |
| Generic user `records` | Every known need has a typed section; raw keys could collide with account or `arkiv_state` keys |
| Segment rows | Segments sit outside the state root and are appended per commit |
| Entities | Not needed for a first genesis; may come later |
| Node settings (`k`, store URL, data directories) | Local configuration, not consensus |

## 6. Decisions

| ID | Question | Proposal |
| --- | --- | --- |
| G1 | Which root goes into the genesis header? | GolemDB's global root: it covers the index as well as the cells. |
| G2 | Which store backend is consensus? | GolemDB. MemStore and Valkey predate it and remain as demonstrations of the store abstraction; they receive explicit genesis pricing for experiments, but do not persist or activate pricing models. |
| G3 | How does a node reject a genesis or a database it cannot run? | GolemDB and Arkiv each expose a function returning the sets of versions they support: GolemDB its `format`s, hash functions, `roaring` versions and GolemDB code versions; Arkiv its host code versions. A genesis whose `encoding` or `@meteringModel` names an unsupported value is refused. On opening an existing (for example prefilled) database, the node checks the superblock and the **active** model against the same sets and refuses on mismatch. A pending model with an unsupported version only warns: GolemDB halts the node at its activation. |
| G4 | Values: cell limits, weights, `arkiv_state` seed (tokenomics O6, O9). | open |

## Implementation plan

Phase 3 pricing persistence, host declarations, version checks and executor
snapshots are implemented through the native format in section 2. Full parameter
support and the other target-layout changes remain planned.

| Phase | Work | Done when | Blocked by |
| --- | --- | --- | --- |
| 1 | **Store-committed genesis on GolemDB.** Run the node on `GolemDbStore`. Parse `arkiv.golemdb` (`encoding`, cell limits in `#params`) in the chain-spec parser. Compute the genesis root without the real store (apply the genesis to an in-memory GolemDB), set the genesis header's `stateRoot` to it, then compute the genesis hash. Change `GolemDbStore::open(directory, genesis)` to take that genesis (settings, accounts from `alloc` in order, genesis hash) as a required argument in place of the hardcoded config: create an empty store from it as one commit 0, check an existing one against its hash. MemStore and Valkey remain experimental (G2). | Same file → same genesis hash on two nodes; changing any `alloc` balance or cell limit → different hash; reopening with a changed file fails. | node on GolemDB |
| 2 | **Accounts end to end.** e2e test: a genesis file funds an address, `eth_getBalance(addr, "0x0")` returns it, and that address can send a transaction. | Test green on GolemDB. | 1 |
| 3 | **Full `#params`, metering and versions.** Accept the remaining `#params` cells, `@meteringModel` and `@modelWeight` as GolemDB implements them; declare Arkiv's `host.*` weight list (host code version 1); add the version functions and checks of G3. | Fee schedule readable from the pricing snapshot at block 0; a genesis or database with an unsupported version is refused. | GolemDB D9 and `#params` |
| 4 | **`arkiv_state`.** Create it from `arkiv.arkiv_state` as the first user record. | Pool balance readable at block 0. | tokenomics implementation, G4 |

Unaffected throughout: `arkiv-cli inject-predeploy` keeps editing `alloc` only.
