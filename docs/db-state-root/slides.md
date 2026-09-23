---
marp: true
theme: default
paginate: true
size: 16:9
style: |
  section { font-size: 24px; }
  section h1 { font-size: 40px; }
  section h2 { font-size: 32px; }
  img { display: block; margin: 0 auto; max-height: 520px; }
  section.two { display: grid; grid-template-columns: 1fr 1fr; gap: 24px; align-items: center; }
  section.two h2 { grid-column: 1 / -1; }
  code { font-size: 20px; }
---

# Arkiv under its own state root

Experiment 2: one database root instead of state interleaved in accounts

Arkiv-Network/arkiv PR #122, issue #120

---

## Agenda

1. Background: persistent trees, Merkle trees, how reth stores state
2. Before: entities and indexes as accounts, dense ids, bitmaps
3. After: three tries under one root in an anchor slot
4. What changed, what is deferred

---

## A persistent tree

A tree that is never modified in place. An update builds a new root and
copies only the nodes on the path to the change. Everything else is shared.

![w:860](diagrams/persistent-tree.svg)

---

## Why persistence matters here

- An update costs the depth of the tree, about `log n` new nodes.
- The old root stays valid. Two versions coexist for free.
- No undo log: to go back, use the old root.
- This is `Data.Map` in Haskell, or a git object store.

The trick is that nodes are immutable, so sharing is safe.

---

## A Merkle tree

Each node is named by the hash of its contents, including its children's
names. The root hash commits to every leaf.

![w:640](diagrams/merkle-tree.svg)

---

## Merkle + persistent

If a node's name is its hash, then:

- storing nodes in a table `hash → bytes` gives a persistent tree for free,
- a root is one 32-byte hash,
- two versions that share subtrees share table rows,
- a proof for a leaf is the path of nodes from the root.

Ethereum's Merkle-Patricia trie is exactly this: a hexary radix trie whose
child references are hashes.

---

## A Patricia trie

A trie stores keys character by character: the path from the root spells the
key, so lookup is one step per character and a walk visits keys in order. A
Patricia trie compresses every run of single-child nodes into one edge.

![w:900](diagrams/patricia-trie.svg)

---

## Ethereum's Merkle-Patricia trie

![w:760](diagrams/merkle-patricia.svg)

---

## How reth stores state

reth keeps flat key-value tables and recomputes the trie for the changed keys
after every block. It does not keep old versions of nodes.

![w:1000](diagrams/reth-tables.svg)

---

## What that means for Arkiv

Anything Arkiv stores in reth must be an account field, a storage slot, or
account code. The state root then commits to it, and every write is one more
account or slot for reth to rehash.

The rest of this deck is about what Arkiv put there, and what it puts there now.

---

# Before this PR

---

## An entity was an account

![w:1100](diagrams/before-entities.svg)

- The 32-byte key is truncated to a 20-byte address.
- The record is stored as account code, behind a `0xFE` marker.
- A system account holds the minting nonces and the id bookkeeping.

---

## The dense id

Every entity got a `u64` id on first sight, allocated from a counter.
Two maps in the system account translated key to id and id to key.

The id existed for one reason: the equality index stored sets of entities as
bitmaps, and a bitmap needs small integers.

---

## The equality index: bitmaps in code

![w:1000](diagrams/before-eq-index.svg)

---

## The range index: a B+ tree in storage slots

![w:1100](diagrams/before-range-index.svg)

- Storage is an unordered slot map, so the order had to be built by hand.
- The tree held distinct values only. Each value still needed its bitmap.
- Strings longer than one word went through a separate cascade structure.

---

## Inserting one entity

![w:420](diagrams/before-insert.svg)

---

## Answering one query

![w:560](diagrams/before-query.svg)

---

## What was awkward, and why

All of it follows from one decision: the database was crammed into Ethereum
accounts, so every structure had to be shaped like an account or a slot.

- Every write touched a dozen accounts and dozens of slots, each rehashed by
  reth's account trie.
- Consensus-critical byte formats scattered across bitmap serialization,
  slot packing and address derivations.
- Two representations of one fact: the bitmap knows which entities, the
  B+ tree knows which values.
- The id maps, the counter, the enumeration lists: bookkeeping that existed
  only to serve the layout.

---

# After this PR

---

## The core idea

Take the database out of the accounts. Keep it as its own persistent
Merkle-Patricia tries in a content-addressed node store. Commit to it with
one root, stored in one storage slot of one account.

The Ethereum state root still covers everything, through that slot.

---

## The layout

![w:900](diagrams/after-overview.svg)

---

## The anchor

- Address `0x61726b69762d64617461626173652d726f6f7421`, the ASCII string
  `arkiv-database-root!`, exactly 20 bytes.
- Slot 0 holds the database root. Nonce 1 keeps the account alive.
- The database root is `keccak(rlp[entities root, nonces root, indexes root])`
  and the top node is stored in the node store too, so it can be read back.

---

## The entities trie

![w:900](diagrams/after-entities.svg)

---

## One trie per index, and one trie of indexes

![w:820](diagrams/after-index.svg)

---

## Inserting one entity: path copying

![w:1000](diagrams/after-insert.svg)

---

## Inside one transaction

![w:380](diagrams/after-commit.svg)

---

## Answering one query

![w:620](diagrams/after-query.svg)

---

## No ids, no bitmaps

- The index key ends with the entity key, so a walk yields keys directly.
- Entities with the same value sit under one prefix, ascending by key.
- `AND`, `OR`, `NOT` are merges of sorted key streams.
- `NOT` and `*` walk the entities trie, which is the live set.

The dense id, the two id maps, the counter, the roaring bitmaps and the
hand-built B+ tree are gone.

---

## History and reorgs

![w:900](diagrams/after-history.svg)

---

## The node store is replaceable

The trie reads through two small traits: `hash → bytes`, and "make this
batch durable". The engine behind them is one file.

The first version used MDBX, the engine reth links. It was then swapped for
turso, driven from synchronous code through a runtime handle like the pruning
map, without touching the trie, the store or the executor. Same roots, same
tests.

---

## Crash safety

A transaction flushes its new nodes to the node store, durably, before it
returns its Ethereum diff. reth persists the block later.

So the node store is always ahead of or equal to reth's database, never
behind. A crash leaves unreachable nodes at worst. Re-executing a block
writes the same hashes again.

Deferred: one shared database transaction with reth, and pruning of unreachable
nodes.

---

## Before and after

![w:1100](diagrams/before-after.svg)

---

## The code

| crate | what |
|---|---|
| `arkiv-trie` | persistent Merkle-Patricia trie over a node store, own path type (keys longer than 32 bytes) |
| `arkiv-store` | the three tries, key encodings, query evaluator, turso node store |
| `arkiv-reth-statemanager` | base shrank to balances, nonces and the anchor slot |
| `arkiv-reth-executor` | reads the parent root, flushes nodes, writes the new root |
| `arkiv-reth-rpc` | reads through the anchor slot of any block's snapshot |

Removed: the account-storage layout crate, genesis seeding.

---

## Verified

- Trie roots checked against alloy-trie's reference builder on random batches.
- Old roots re-read after every batch: persistence holds.
- Whole workspace passes, including the node's end-to-end suite on the real
  binary: creates, every query operator, pagination, historical reads,
  expiry purging, kill and restart.

Behavior changes: results ascend by entity key; the page cursor is an offset
bound to a block.

---

## Deliberately out of scope

Left out of the proof of concept on purpose. None of them change the root
format, so each is a follow-up, not a fork.

- Crash atomicity with reth through one shared database transaction.
- Pruning unreachable nodes.
- Snap sync of the node store for followers.
- Seeding at genesis on the new layout.
- Committing the root once per block instead of once per transaction: today
  every transaction rebuilds the upper trie nodes, the index-of-indexes path
  and the top node on its own. Batching a block's changes would rebuild the
  shared upper nodes once. The block's root is identical either way.

---

## Take-aways

- reth stores a database state root: one storage slot of one account.
- The database data, entities, nonces and indexes, is stored entirely
  outside of reth, in a turso database.
- Gas calculations and estimates still work.
- The data in turso is reorg safe: every root ever written stays readable,
  and the canonical anchor slot names the canonical root.
