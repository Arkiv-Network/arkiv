# Typed attributes and the query index, from the ground up

How an attribute value's *type* travels from the wire into storage, how it
decides which index structures the value lands in, and how a query finds it
again. Written for an engineer new to the codebase; every claim points at the
crate that owns it.

## 1. The type system

An attribute is a `(name, value)` pair on an entity. The value is not loose
bytes: it is an `AttributeValue` (`crates/arkiv-interfaces/src/entity.rs`), a
tagged union where the tag — the **typeId** — is part of the value itself and
travels with it on the wire, in storage, and in every index key.

| typeId | variant | ABI type | indexing |
|---|---|---|---|
| 1 | `Bool` | `bool` | equality |
| 2 | `Int` (i32) | `int32` | equality + range |
| 3 | `U256` | `uint256` | equality + range |
| 4 | `Decimal` | `int256` | equality + range |
| 5 | `Bytes32` | `bytes32` | equality |
| 6 | `Bytes` | `bytes` | none — system-only (`$payload`) |
| 7 | `Str` | `string` | equality + prefix |
| 8 | `EthereumAddress` | `address` | equality |
| 9 | `EntityKey` | `bytes32` | equality |

The typeId numbering is **consensus-critical**: it is written into entity
records and mixed into index-account addresses, so renumbering forks the chain.
It is pinned by `type_ids_match_the_spec` in `arkiv-interfaces`.

`Decimal` is a fixed-point number: a two's-complement 256-bit integer scaled by
a fixed 18 decimal places (`DECIMAL_SCALE`), so `1.5` is stored as
`1_500_000_000_000_000_000` and any two decimals compare as plain integers.

### Two byte encodings, and why there are two

Every value has *storage* bytes and *index* bytes, both defined on
`AttributeValue`:

- **`encode()` — storage bytes.** The value at its natural width, big-endian,
  no tag (the typeId travels alongside). `decode()` is the exact inverse and
  validates on the way back in (bool must be 0/1, str must be UTF-8, fixed
  widths must match).
- **`index_bytes()` — index bytes.** Identical to `encode()` *except* for the
  signed types (`Int`, `Decimal`), whose sign bit is flipped. Two's complement
  puts negatives *above* positives in byte order; flipping the sign bit biases
  the value by half the range, which restores `byte order == numeric order`.
  That single property is what makes a range scan over a plain byte-ordered
  structure correct, so this encoding is consensus-critical too.

Rule of thumb: storage bytes are for reading a value back; index bytes are for
finding it. Never mix them.

There is actually a third encoding, upstream of both: the **ABI wire form**.
On calldata an attribute is `{ Ident32 name; uint8 valueType; bytes32[4] value }`
(`crates/arkiv-bindings`) — the typeId as an explicit byte, the value in up to
four 32-byte words (single-word types right-aligned in word 0, strings
left-aligned across all four). The executor's decode validates the padding and
converts to `AttributeValue`; from there the storage record keeps
`(name, typeId, encode())` (RLP, in the entity account's code) and the index
keys on `(name, typeId, index_bytes())`. Three encodings, one type system.

## 2. Where index data physically lives

The index is ordinary EVM state — accounts and storage slots — so it commits
through the same state root as everything else. Each index bucket is an
account whose address is derived by hashing a domain-tagged preimage
(`crates/arkiv-reth-auxstore/src/address.rs`). The five derivations:

```
pair account   keccak("arkiv.pair" || attr || 0x00 || typeId || index_bytes)[..20]
btree header   keccak("arkiv.ibth" || attr || 0x00 || typeId)[..20]
btree node     keccak("arkiv.ibtn" || header_addr || node_id_be)[..20]
cascade level  keccak("arkiv.sidx" || attr || 0x00 || typeId || prefix)[..20]
enum list      keccak("arkiv.list" || index_addr)[..20]
```

Three things to notice, because they are the type system showing up in the
addressing:

- **The typeId is in every derivation.** One attribute name holding values of
  two types gets two fully disjoint sets of buckets. A `bool true` and a
  `u256 1` under the same name can never answer each other's queries; a
  predicate typed `i32` can never match a `u256`-typed attribute. "A value
  predicate asserts the attribute exists *with this exact type*" is not
  enforced by a runtime check — it falls out of the addressing.
- **The `0x00` separator** stops the attr/value split from sliding
  (`("ab","c")` vs `("a","bc")`), which is why attribute names must not
  contain a zero byte.
- **These addresses are pinned by golden test vectors.** Moving any tag or
  byte order orphans every bucket already on chain — a hard fork. The tests in
  `address.rs` exist to make that impossible to do silently.

## 3. The two-tier index

### Tier 1 — equality bitmaps (every indexed type)

Every distinct `(attr, typeId, value)` triple has a *pair account* whose code
is a roaring64 **bitmap of entity ids** — the set of entities currently
carrying exactly that pair (`bitmap.rs`). Bitmaps are keyed on a compact `u64`
entity id, not the 32-byte entity key; the store owns the key↔id maps and
allocates ids densely in creation order (`store.rs`).

One special bucket, `($all, "")`, holds every live entity. It is how `*`
queries and every negation are answered: `NOT p` = `$all` minus `p`'s bitmap.

Bitmap serialization is deterministic and the crate is version-pinned, because
a pair account's `codeHash = keccak256(bitmap_bytes)` — two nodes holding the
same set must produce the same bytes or their state roots diverge.

Tier 1 alone answers `=`, `!=`, and boolean combinations: read bitmaps,
intersect/union/subtract (`interpret.rs`).

### Tier 2 — ordered structures (only the ordered types)

Range and prefix operators need to enumerate *which values exist* in order —
a bitmap per value can't do that. So for ordered types the index additionally
records each distinct value in an ordered structure, one per `(attr, typeId)`:

- **Numeric types (`i32`, `u256`, `dec`) → a B+ tree** over storage slots
  (`btree.rs`, `range.rs`). The tree key is the value's index bytes
  left-aligned in a 32-byte slot; byte order equals numeric order (that's what
  the sign-bit bias bought us). A `level >= i32(10)` scan walks the tree from
  the bound, collecting matching values.
- **Strings → a chunk cascade** (`cascade.rs`), because a B+ tree slot caps at
  32 bytes and strings go to 128. A value is split into 32-byte chunks; level
  0 stores chunk 0, level 1 lives at an address derived from chunk 0 and
  stores chunk 1, and so on. A prefix scan walks levels depth-first,
  reconstructing values. EVM storage can't be enumerated, so every level
  account carries a companion *enumeration list* recording its distinct
  chunks.

Which structure an attribute uses is decided in exactly one place —
`capabilities_for` in `annotation.rs` — shared by the write and read paths so
they cannot disagree:

| capability | types | tier-2 structure |
|---|---|---|
| `Equality` | bool, bytes32, addr, key | none |
| `EqualityAndRange` | i32, u256, dec (+ `$expiration`, `$createdAtBlock`) | B+ tree |
| `EqualityAndPrefix` | str (+ `$contentType`) | cascade |
| `None` | bytes (`$payload`) | not indexed at all |

Built-in (`$`-prefixed) attributes are classified by *name*; user attributes
by their value's *type*. This table is the spec's "index" column, realized.

Tier 2 stores no entity ids — only values. A range scan first collects the
matching values from tier 2, then resolves each one to its tier-1 pair bitmap
and unions those. That two-step resolution is why every value is recorded
twice.

## 4. The write path

The executor decodes an operation from calldata, applies it to the entity
store, and diffs the entity's full annotation set (seven built-ins + user
attributes, `entity_annotations` in `annotation.rs`) before and after. The
diff becomes an `AuxiliaryEntityDelta` — typed inserts and removes — folded
into the index by `index.rs`:

- **Insert:** read the pair bitmap, set this entity's bit, write it back. If
  the bitmap was empty, this value just came into existence for this
  attribute: record it in tier 2 (if the type is ordered).
- **Remove:** clear the bit. If the bitmap is now empty, the value no longer
  exists anywhere: drop it from tier 2 (lazily — tier-2 deletion marks the
  entry absent; readers skip it).

So tier 1 tracks *entities per value* and changes on every write; tier 2
tracks *distinct values* and only changes when a value's population crosses
zero.

## 5. The read path

`interpret::eval` walks the query AST to a single bitmap of entity ids:

- `=` → read one pair bitmap (the query value's own typeId and index bytes
  derive the address — identical to what the writer derived, guaranteed by
  sharing `annotation.rs`).
- range → tier-2 scan for values, union their pair bitmaps.
- prefix → cascade scan, same resolution.
- `AND`/`OR` → intersect/union; `NOT`/`!=` → subtract from `$all`.

The store then pages the surviving ids newest-first (descending id, with the
cursor an exclusive upper bound), maps ids back to keys, and the RPC layer
loads the full entities and applies BTL/expiry filtering.

## 6. What the typed system changes, in one paragraph

In the old model a value was bytes and a number was "a u256"; queries matched
whatever bytes collided. In the typed model the typeId is part of the value's
identity end to end: the ABI carries it, the record stores it, every index
address hashes it, and a predicate carries it too. The practical consequences:
same-named attributes of different types are disjoint (no cross-type matches,
ever); signed types (`i32`, `dec`) are range-scannable because their index
encoding is sign-bias corrected; strings get real prefix indexing; `bytes` is
deliberately unindexable; and adding a future type means assigning a typeId
and a `capabilities_for` row — the addressing and both tiers already
generalize over `(attr, typeId, index_bytes)`.
