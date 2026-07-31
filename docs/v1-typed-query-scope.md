# Typed query language: a proposal for trimming V1 scope

This is a proposal, not a decision — I'd like us to align on it together.
The short version:

- **Suggest cutting from the spec:** `exists()`, `typeof()`, and `!=`. All
  three need a new consensus index structure that I think deserves more
  design time than V1 allows. `NOT (…)` stays and covers the practical need.
- **Suggest deferring:** filtering on `$updatedAt`, and `creationFlags`.
  Both hinge on decisions we haven't made yet, and both can be added later
  without breaking anything.
- **Good news:** `patchEntity` needs no new work at all — it can ship in V1
  as an SDK wrapper over `update`.
- Everything else in the spec (tagged literals, `STARTSWITH`, projections,
  cursors, error codes) looks good to me and ships as written.

The reasoning behind all of these is the same trade-off: parser and RPC
changes are easy to fix in a later release, but index buckets, record
formats and ABI ops are consensus state — a mistake there means a fork or a
full reindex. So my suggestion is that V1 ships syntax generously and
consensus state conservatively.

## Background: how a query is answered (60 seconds)

For the examples, entities are people in a phone directory. Short service
codes like 911 fit an `i32`, and many officers share one line. Full 10-digit
numbers don't fit an `i32` (it tops out around 2.1B), so those go in a
`u256`. One attribute name, two types, both legitimate.

The index has two tiers:

- **Tier 1:** one bucket per `(attribute, type, value)` triple, holding the
  set of entities with exactly that value. `tel = i32(911)` is one bucket
  read. `AND`/`OR` intersect and union these sets.
- **Tier 2:** for orderable types, a sorted structure over the *values* that
  exist (a B+ tree for numbers, a prefix tree for strings). A range query
  like `tel < i32(1000)` walks the tree to find which values are in range
  (100, 112, 911, 999), then unions their tier-1 buckets.

Two facts carry the rest of this doc:

1. **Every bucket is keyed by a value.** Nothing is keyed by attribute alone.
2. **Buckets are consensus state** — accounts in the state trie, at addresses
   hashed from `(attribute, type, value)`. Adding or moving a bucket family
   is a change every node must make in lockstep.

## Proposal 1: cut `exists`, `typeof`, `!=` (for now)

What the spec asks for, on this directory:

| entity | attributes |
|---|---|
| Alice | `tel: i32(911)` |
| Bob | `tel: u256(4155550123)` |
| Carol | `tel: i32(112)` |
| Dave | `remote: true` (no tel) |

```
exists(tel)           → Alice, Bob, Carol   set with any type
typeof(tel) = u256    → Bob                 the full-length numbers
tel != i32(911)       → Carol               has an i32 tel, and it differs
NOT (tel = i32(911))  → Bob, Carol, Dave    complement over everyone
```

These are genuinely useful queries — that's not in question. The problem is
that all three are *presence* questions ("is `tel` set, as this type?") with
no value to look up, and per fact 1 the index has nothing to read. Supporting
them means a new bucket family: a presence bitmap per `(attribute, type)`,
e.g. `(tel, i32) → {Alice, Carol}`. The structure itself is simple; my
hesitation is about what surrounds it:

- **Cost lands on everyone.** It's maintained on every attribute write,
  forever, while the common SDK pattern (filtering by your own known
  attributes) never issues these queries. That trade might still be worth it
  someday — I just don't think we have the evidence yet.
- **The write path is subtle.** Alice moving from `i32(911)` to `i32(112)`
  emits "remove 911, insert 112" — a naive presence update drops her bit and
  re-adds it, which is only correct while both halves are processed
  together. Consensus code with invariants like that deserves unhurried
  review, and V1's timeline doesn't offer it.
- **We haven't designed index cleanup yet.** Expired entities leave their
  bits behind (expiry is passive — no transaction runs), and parts of the
  index already grow lazily. I'd rather we design index GC first (who
  sweeps, what compacts, what the growth budget is) and let presence bitmaps
  be decided inside that conversation, where their cleanup story gets solved
  along with everything else's.

**Why I'd let `!=` go with them.** Without presence bitmaps, `!=` could only
mean the complement — "different *or absent*", which would include Dave, who
has no phone at all. If we ship that meaning and later add the typed
semantics, every stored `!=` query silently changes its results — a door I'd
rather not close. Leaving `!=` out keeps both futures open, and we lose
little: `NOT (tel = i32(911))` expresses the complement with its meaning
visible in the query. A nice side effect is that the language keeps one
uniform rule with no exceptions: every value predicate asserts the attribute
exists with that exact type.

**Why `NOT` is unaffected.** `NOT` only needs one global universe to
subtract from, and the `$all` bucket has always provided it. The features
above need a universe per `(attribute, type)` — that's precisely the missing
piece.

To keep the door open, I'd keep `EXISTS` and `TYPEOF` as reserved words
(costs nothing) and leave `!=` unassigned. If demand shows up, presence
bitmaps can land additively later — new buckets, nothing moves, backfill by
replaying history.

## Proposal 2: defer `$updatedAt` filtering

`$updatedAt` changes on every operation, so indexing it makes every update,
extend and transfer pay index churn forever — and since nearly every entity
has a distinct value, range scans over it degenerate to one bucket read per
matching entity, the least favorable shape tier 2 supports. Reading
`updatedAt` via projection still works; only the server-side filter waits.
Anyone who needs the filter today can maintain their own `updated`
attribute, which indexes normally. If a strong use case appears, this is one
more built-in index and an eyes-open acceptance of the write cost.

## Proposal 3: defer `creationFlags`

The entity model has no flags field yet, so even the projection needs a
record format bump, an ABI change, and — the real open question — agreement
on what `readonly` actually forbids. I'd suggest we settle those semantics
first and freeze bits second; the record format is versioned exactly so this
can land later as a v2 record without disturbing anything.

## Good news: `patchEntity` needs nothing

`patchEntity` turns out to be `update` in disguise. Since `update` replaces
payload, contentType and the whole attribute set, the SDK can implement
patch as: read the entity → apply `set`/`unset` to the attribute map → send
it back as an `update`. The result is identical, and `unset` needs no
tombstone because omitted *is* removed. Helpfully, only the owner can update
an entity, so the read-modify-write race is confined to your own processes.
A native patch op later would save the read round-trip and some gas — a
clean, additive optimization whenever we want it.

## Stop-gaps, in one table

| you wanted | in the meantime |
|---|---|
| `exists(a)` | range types: `a >= i32(-2147483648)` etc. · bool: `a = true OR a = false` · str: `a STARTSWITH str('')` · addr/key/bytes32: project `attributeSchema`, filter client-side |
| `typeof(a) = T` | same tricks — or keep one type per name in your own schema and the question disappears |
| `a != v` | `NOT (a = v)` — the complement, stated explicitly |
| filter on `$updatedAt` | filter on `$createdAt`/`$expiresAt`, or maintain your own `updated` attribute |
| `creationFlags` | convention-level flags in your own attributes; no immutability guarantees yet |
| `patchEntity` | nothing needed — the SDK ships it over `update` |

## Why raise this now

All of this is cheap while the devnet can still be regenesised, and expensive
the day a chain has state worth keeping. The idea is to front-load only the
irreversible decisions — reserved words, versioned formats, `!=` left
unassigned — and postpone the reversible work. If we agree, the spec
appendix needs a matching edit (drop `exists`/`typeof`/`!=` from the
grammar, keep the words reserved) before the SDK team freezes their
query-builder API.

Happy to talk through any of these — especially if someone has a concrete
use case for `exists`/`typeof` that the stop-gaps don't cover, that would
genuinely change the calculus on the presence bitmaps.
