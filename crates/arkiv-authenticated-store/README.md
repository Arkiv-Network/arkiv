# Authenticated Arkiv chain state

The node stores Arkiv-specific data in an ordered authenticated KV store.
One Ethereum system-account storage slot commits to its root. Native balances,
transaction nonces, and Ethereum protocol accounts stay in normal Ethereum state.
It defines a fresh chain layout and does not import or migrate old account-backed
Arkiv state.

## Run the blockchain

```sh
cargo run -p arkiv-reth -- node --dev --datadir /tmp/arkiv-new-chain --http
```

Use a fresh datadir. The existing transaction ABI and `arkiv_*` JSON-RPC methods
are served by this backend. The dev miner produces blocks locally; the existing
Kurtosis/Lighthouse setup supplies consensus for a multi-node network.

```sh
cargo run -p arkiv-cli -- seed-genesis --count 1000 --out /tmp/arkiv-genesis.json
cargo run -p arkiv-reth -- node --dev --chain /tmp/arkiv-genesis.json --datadir /tmp/arkiv-seeded --http
cargo test -p arkiv-authenticated-store -p arkiv-reth-statemanager -p arkiv-reth-executor -p arkiv-reth
```

The smaller storage-only example remains available:

```sh
cargo run -p arkiv-reth-statemanager --example authenticated_state -- /tmp/arkiv-tree 1000
```

## Node integration

`ChainView` is the live state-manager implementation. The executor, payload
builder, RPC, and offline block import/re-execution share an explicit record-store
handle. Records persist in `<resolved datadir>/arkiv-state`, alongside reth's
native databases. There is no separately maintained canonical Arkiv head.

Every transaction stages its entities, indexes and creation nonces together.
Records become durable before the new root enters the Ethereum account diff.
Received blocks execute the same transition and reth checks the resulting outer
state root. RPC selects the custom root from the requested Ethereum snapshot.
Historical queries and competing forks therefore read their own immutable trees.

Purge selection traverses the parent's authenticated expiration index with key
and gas limits. No SQLite pruning map, log replay or genesis index bootstrap is
needed. Entity deletion updates all relevant buckets in the same commitment.

A seeded genesis contains `config.arkivState = {root, records}` and its native
`alloc` contains the root account plus funding/predeploys. The importer verifies
hashes, graph completeness, canonical tree structure, and the root commitment
before publishing records. JSONL/native `stateHash` imports retain the existing
native-state validation; custom records travel in the accompanying genesis.
For alloc-only packaging, the seeder also writes `<out>.arkiv.json`. Set
`ARKIV_GENESIS_STATE` to that file on every node; the chain parser attaches it as
`config.arkivState`. The Kurtosis script mounts it for all EL participants using
[extra files](https://github.com/ethpandaops/ethereum-package#extra-files-and-mounts).

Back up the entire datadir, including `arkiv-state`. A node refuses to start if
its selected root record is missing. Full block execution rebuilds custom state
from genesis, including offline `import`; an Ethereum account-only snapshot is
insufficient. Custom snapshot networking is not implemented.

## Commitment and record format

```text
Ethereum header.stateRoot
  -> root account 0x4400000000000000000000000000000000000046
     -> storage[keccak256("arkiv.authenticated.root.v1")]
        -> ordered Merkle tree root
```

An absent/zero root slot denotes an empty custom tree. The root account's nonce
is raised to one when first written, to keep a storage-only account alive. A
read-only operation or native-only change does not rewrite the root slot.

The custom tree is a **Merkle treap** (binary search tree with hash-derived heap
priorities). Keys remain ordered bytes.
Node priorities are `(keccak256("arkiv.tree.priority.v1" || key), key)`, including
a deterministic tie-breaker. Consequently the same key/value map has the same
shape and root regardless of mutation order. Internal entity IDs still depend
on creation order, as specified by their allocation policy.

The dedicated MDBX table `ArkivAuthenticatedRecordsV1` maps record hashes to
bytes. A node has the unique encoding:

```text
"arkiv.tree.node.v1" || key || value_hash[32] || left_hash[32] || right_hash[32]
```

Values are stored separately as `"arkiv.tree.blob.v1" || value`; both record kinds
are addressed by Keccak-256 of their complete encoding. Fixed-width trailing
fields make node decoding unambiguous. Zero denotes an absent child. Reads check
record hashes and fail on missing/corrupt data. Separating values avoids copying
entity payloads into every rewritten ancestor. Merkle membership/range proof
APIs are not implemented.

## Logical namespaces

| Prefix | Remaining key | Value |
| --- | --- | --- |
| `0` | Full 32-byte entity key | Existing entity-record codec bytes |
| `1` | Attribute length (u32 BE), attribute bytes, type ID, ordered value bytes | Serialized Roaring64 bitmap |
| `2` | Full entity key | Internal ID (u64 BE) |
| `3` | Internal ID (u64 BE) | Full entity key |
| `4` | Empty | Next internal ID (u64 BE) |
| `5` | Creator address (20 bytes) | Entity-creation nonce (u64 BE) |

This crate reuses the existing entity codec, annotation extraction, type
classification, and Roaring codec. It does not use their account addresses or
storage-slot layouts. Entity payloads, attributes, lifecycle metadata, built-in
index entries, `$all`, and ID bookkeeping are all authenticated. Expiration selection uses the authenticated expiration buckets directly.

An ordered bucket stores its bitmap directly as the value. Numeric range queries
seek/traverse the requested interval and union only the matching buckets. Strings
use the same tree, with prefix queries expressed as a byte interval; there are
no chunk cascades or enumeration-list accounts. A bounds-aware traversal reads
boundary paths and matching nodes, rather than materializing the entire suffix.
The low-level scan callback can stop immediately. The query API still builds
the full matching bitmap before paging in descending entity-ID order; age order
does not imply newest-first order. Boolean `And` currently evaluates its
operands separately; callers can use `State::range` for a single two-sided scan.

## Persistence and forks

Updates copy only changed paths and stage new records in memory. `persist()`
selects reachable pending records and writes them in one MDBX transaction;
intermediate roots within a batch are not retained. Persisting during an open
rollback scope is rejected. Missing keys are distinct from zero-valued records,
so ID zero and zero numeric values do not need sentinel encodings.

The adapter persists records **before** returning the Ethereum diff. There is
no custom mutable "latest root" to race with competing proposals: the Ethereum
view selects the root. A crash/abandoned proposal after persistence can leave
unused records, but must not leave a published Ethereum root whose records were
never persisted. The live executor preserves this ordering, retaining all roots needed by
in-flight payloads and reorgs. Old persisted roots remain
readable; no disk garbage collector is implemented.

`State::transaction` rolls back Arkiv changes on errors. The adapter discards
both Arkiv and native overlays if its callback fails. Business rules, fees for
reverted transactions, and deciding when to apply the returned diff belong to
the executor; the example's balance changes are illustrative.

## Remaining performance and operational work

- A hash-priority treap has expected logarithmic height, not a worst-case bound
  for adversarially ground keys. Mutation is recursive. Choose a bounded-depth
  structure or enforce resource limits before production.
- Bitmaps are whole serialized values; hot buckets still rewrite the whole
  bitmap. Chunking/sharding and higher-fanout nodes remain experiments.
- MDBX reads currently open a read transaction per record. Reusing a snapshot
  transaction and caching verified records should be measured before comparing
  throughput with the current backend.
- Pending intermediate records consume memory until `persist()`. Production
  needs a bounded buffer, durable staging, and garbage collection.
- No custom proof API, snapshot networking, or disk garbage collector is included.
  Gas costs retain the existing model and have not been calibrated to this tree.
- Genesis snapshot export/validation holds the snapshot in memory. The JSONL native
  dump is streamed, but custom genesis construction is not a bounded-memory seeder.

Tests cover canonical roots against a reference ordered map, updates/deletes,
bounded/early-stop scans, typed queries and overlapping string prefixes, full
entity-key preservation, ID/nonce rollback, MDBX reopen/forks, corruption/write
failures, and the separation of native accounts from custom state.
