# Arkiv Node API

The client-facing surface of an Arkiv node: type vocabulary, the write ABI, the
read/query JSON-RPC, events and errors.

## 1. Overview & namespaces

The node speaks **two JSON-RPC namespaces on one endpoint**:

- **`eth_*` — standard Ethereum.** Transaction submission
  (`eth_sendRawTransaction`), receipts/logs, blocks, and proofs
  (`eth_getProof`). Account reads (`eth_getBalance`, `eth_getTransactionCount`)
  are served from Golem DB account state.
- **`arkiv_*` — Arkiv reads.** `arkiv_query` (the typed query language, §4/§5)
  and the entity/account **view** reads (§5).

**Writes** go through the standard tx envelope: a transaction whose `to` is a
known Arkiv-operation address and whose calldata is the ABI-encoded
`execute(Operation[])` payload (§3), submitted via `eth_sendRawTransaction`.
Every tx whose `to` is not an Arkiv-operation address **reverts**. **Reads**
never enter a transaction; queries are served by **watchers**, untrusted for
results and verified client-side (predicate match + membership proof against
the block's `state_root`).

**Chain quantities** (block heights, limits, counts) are `0x`-prefixed hex
strings everywhere, matching the `eth_*` namespace.

## 2. Type vocabulary

The client/query surface uses the short **tag**; the write ABI (§3) uses the
Solidity **wire type**. They map 1:1 by `typeId`.

| tag | typeId | ABI wire type | query literal grammar | index (when flagged) |
|---|---|---|---|---|
| `bool` | 1 | `bool` | bare `true` / `false` (no wrapper) | eq + range |
| `i32` | 2 | `i32` (`int`) | `i32(n)` — sign + digits, range-checked [−2³¹, 2³¹−1]; also the **default** for an untagged number literal | eq + range |
| `u64` | 3 | `u64` (`uint64`) | `u64(n)` — decimal or `0x` hex, ≤ 2⁶⁴−1; also counters/timestamps and the protocol block heights (`$createdAt`, `$updatedAt`, `$expiresAt`) | eq + range |
| `u256` | 4 | `u256` | `u256(n)` — decimal or `0x` hex, ≤ 2²⁵⁶−1; large unsigned integers (token amounts, etc.) | eq + range |
| `dec` | 5 | `i256` (fixed 18 dp) | `dec(n)` — sign, digits, `.` + ≤ 18 fractional digits; no exponents; excess precision = parse error, never rounded | eq + range |
| `bytes32` | 6 | `bytes32` | `bytes32(0x…)` — exactly 64 hex | eq + range |
| `bytes` | 7 | `bytes` (≤ 128 KiB) | — (never indexed, so never queryable) | none — the indexed flag is rejected |
| `str` | 8 | `string` (≤ 128 B) | `str('…')` — single-quoted, `''` escapes, ≤ 128 B UTF-8 | eq + range + prefix |
| `addr` | 9 | `address` | `addr(0x…)` — 40 hex; all-lower, all-upper, or valid EIP-55; bad mixed-case checksum = parse error | eq + range |
| `key` | 10 | `bytes32` (`entity_key`) | `key(0x…)` — exactly 64 hex | eq + range |

`dec` is a scaled `i256` (value × 10¹⁸).

### Indexed and unindexed attributes

An entity is a fixed set of engine-written **system attributes** (§4) plus
**any number of user attributes**, each a `(name, type, indexed)` triple. No
user attribute is mandatory and no user name is special; a blob is a `bytes`
attribute, and an entity may have zero or more of them.

On the wire the indexed flag is **bit 7 of the `typeId` byte**; the low seven
bits are the type from the table above. So `str` travels as `0x08` unindexed and
`0x88` indexed.

| | indexed (bit 7 = 1) | unindexed (bit 7 = 0) |
|---|---|---|
| types | any type except `bytes` | any type, including `bytes` |
| stored and returned in reads | yes | yes |
| usable in a query predicate | yes, with the ops in the index column | no |

- `bytes` is **unindexed only**: the engine rejects `bytes` with the indexed
  flag set.
- The indexed flag belongs to the attribute as written: a `patch` that rewrites
  an attribute may flip it between indexed and unindexed, as it may change its
  type.

### JSON representation

The JSON encoding of a value follows its **declared type**, never its magnitude
(a client dispatches on `type`), and is the same whether or not the attribute is
indexed:

| tag | JSON encoding |
|---|---|
| `bool` | JSON bool |
| `i32` | JSON number |
| `u64` | QUANTITY — `0x` hex, compact (no leading zeros; `0` = `"0x0"`) |
| `u256` | QUANTITY — `0x` hex, compact (no leading zeros; `0` = `"0x0"`) |
| `dec` | decimal string (`"3.5"`) |
| `bytes32` | DATA — `0x`, fixed 32 B |
| `addr` | DATA — `0x`, fixed 20 B |
| `key` | DATA — `0x`, fixed 32 B |
| `bytes` | DATA — `0x`, variable |
| `str` | JSON string |

`i32` is the only integer carried as a JSON number (it fits JS's 2⁵³ safe
range); `u64` and `u256` are compact `0x` QUANTITY strings (a small block height
is `"0x191"`). QUANTITY vs DATA is the standard `eth_*` split: numbers compact,
byte-arrays fixed-width.

## 3. Write path — the `execute` ABI

One external entry point. A transaction is an ordered, **atomic** batch of entity
ops (all-or-nothing; op *n+1* observes op *n*'s effects; one event per applied
op). Canonical encoding of `operationData` is required.

```typescript
/// @return keys the entity key created/affected by each op.
function execute(Operation[] calldata ops)
    external
    returns (EntityKey[] memory keys);

/// Tagged union: `operation` selects the payload struct that `operationData`
/// ABI-decodes to. Canonical encoding required; unknown tag ⇒ typed revert.
struct Operation {
    uint8 operation;     // CREATE=1 PATCH=2 EXTEND_EXPIRY=3
                         // TRANSFER_OWNERSHIP=4 DELETE=5
    bytes operationData; // abi.encode(<payload struct below>)
}

// Expiry (create + extend_expiry) — block heights are uint64 on the wire with a
//   protocol-bounded domain: expiresAt <= MAX_EXPIRES_AT and minLifetime <=
//   MAX_LIFETIME (both ProtocolParams; typed revert above, never truncation).
//   expiresAt == MAX_EXPIRES_AT ⇒ permanent (exempt from the lifetime bound).
//   Otherwise: target = max(expiresAt, current + minLifetime);
//   require target > current and target <= current + MAX_LIFETIME.
//   extend_expiry additionally requires target >= $expiresAt (> extends,
//   == no-op, < reverts).

struct Create {
    uint128 salt;           // caller-chosen key salt; 0 = predictable key
    uint64 expiresAt;       // absolute target block height; 0 = pure relative;
                            // MAX_EXPIRES_AT = permanent
    uint64 minLifetime;     // relative floor in blocks; 0 = no floor
                            // (pure absolute)
    uint8 creationFlags;    // bit0 readonly, bit1 permissionless_extension,
                            // bits 2-7 reserved (must be zero)
    Attribute[] attributes; // strictly ascending by name; no tombstones;
                            // may be empty
}

struct Patch {
    EntityKey entityKey;
    Attribute[] mutations;  // strictly ascending; tombstones allowed; non-empty.
                            // A mutation replaces the attribute whole — type,
                            // indexed flag and value.
}

struct ExtendExpiry {
    EntityKey entityKey;
    uint64 expiresAt;       // same domain + resolution as Create
    uint64 minLifetime;
}

struct TransferOwnership {
    EntityKey entityKey;
    address newOwner;       // non-zero and != current owner
}

struct Delete {
    EntityKey entityKey;    // owner-only, active entities
}

/// The one wire shape shared by attributes (create) and mutations (patch).
struct Attribute {
    Ident32 name;  // name grammar: see §4 (Attribute names) — case-sensitive
    uint8 typeId;  // bit 7: indexed flag (1 = indexed, 0 = stored only)
                   // bits 0-6: 1 bool | 2 int(i32) | 3 uint64 | 4 u256
                   //   5 decimal(i256) | 6 bytes32 | 7 bytes (unindexed only)
                   //   8 string (<=128 B) | 9 address | 10 entity_key
                   // 0x00 exactly: tombstone (patch only)
    bytes value;   // ABI encoding selected by the type bits: exact 32-byte word
                   // for word types, raw bytes for string/bytes (bytes <= 128 KiB),
                   // zero-length for tombstone
}
```

**Size caps.** Each `bytes` attribute is capped at 128 KiB. The per-entity cap
(total bytes per entity, or an attribute-count cap) is open.

**Key derivation** (client-side, for same-batch composition — create then target
the predicted key):

```typescript
entityKey = keccak256(abi.encodePacked(domain, owner, nonce, salt));
// domain: chain-config constant (ProtocolParams.DOMAIN)
// nonce:  per-owner entity-nonce counter, queryable via entityNonce(owner)
//         — guarantees uniqueness even when salt repeats or is 0
// salt:   caller-chosen uint128; unpredictability only. salt = 0 ⇒ predictable
//         key; random salt ⇒ only the creator can predict the key
```

**Events** — one per applied op, in application order: `EntityCreated`,
`EntityPatched`, `ExpiryExtended`, `OwnershipTransferred`, `EntityDeleted`.
Purge emits **no** event; clients track the indexed `$expiresAt`.

## 4. Read path — the `arkiv_query` language

The `arkiv_query` string is a bare boolean expression — **no `WHERE`** — or the
**match-all** query `*`. Keywords are case-insensitive; `--` comments to end of
line.

Queries see only **indexed** user attributes and the queryable system
attributes. Unindexed attributes are stored and returned in results, but no
predicate can reach them.

### Typed literals

See the query-literal column of the §2 type table. Examples:

```sql
    level       >= i32(10)
AND balance     >  u256(1000000)
AND score       BETWEEN dec(3.5) AND dec(5)
AND name        =  str('Bob')
AND desc        STARTSWITH str('ab')
AND parent      =  key(0x123…)      -- 64 hex
AND hash        =  bytes32(0x456…)  -- 64 hex
AND flagged     =  true
AND $owner      =  addr(0xAbC…)     -- 40 hex
AND $expiresAt  <  u64(1200000)     -- system block height: u64, explicitly tagged
AND exists(reviewedBy)              -- set and indexed, with any type
AND typeof(age) =  i32
```

### Rules

- A value predicate implicitly asserts "attribute exists, **indexed**, with this
  exact type." `level >= i32(10)` never matches a `u256`-typed `level`, nor an
  unindexed `level`.
- `!=` is **typed value-negation** (attribute set and indexed as that type,
  value differs). The **full complement** is `NOT (…)`, evaluated against the
  active-entity set — so it also matches entities where the attribute is
  unindexed or unset. `NOT`/`!=`-complement queries need a complete index to be
  served soundly by an untrusted watcher.
- `a BETWEEN lo AND hi` is **inclusive** at both ends: it matches exactly what
  `a >= lo AND a <= hi` matches. Both bounds must have the same type tag; mixed
  bounds are a type error. `lo > hi` matches nothing. The `AND` inside
  `BETWEEN` belongs to it, not to the boolean conjunction: `a BETWEEN 1 AND 5
  AND b = 2` is `(a BETWEEN 1 AND 5) AND b = 2`. The complement is
  `NOT (a BETWEEN lo AND hi)`.
- `STARTSWITH` is the only pattern operator (raw UTF-8 byte prefix, **no
  normalization**).
- **Untagged number literals default to `i32`** (`level >= 10` ≡
  `level >= i32(10)`). Every other type — including the `u64` system block
  heights — must be **explicitly tagged** (`$expiresAt > u64(1443322)`,
  `$owner = addr(…)`).
- `exists(name)` is true if the attribute is set **and indexed**, with any type.
  `typeof(name) = tag` likewise only sees indexed attributes.
- **Match-all:** the query string `*` matches **every active entity** at
  `atBlock`, still paged by `limit`/`cursor`. `*` is only valid as the
  **entire** query (`* AND level > 10`, `NOT *` are parse errors).

### Grammar

```ebnf
query       = matchAll | expr ;
matchAll    = "*" ;
expr        = andExpr , { OR , andExpr } ;
andExpr     = unaryExpr , { AND , unaryExpr } ;
unaryExpr   = NOT , unaryExpr | primary ;
primary     = "(" , expr , ")" | predicate ;

predicate   = comparison | rangeTest | prefixTest | existsTest | typeTest ;
comparison  = attrRef , compOp , value ;
compOp      = "=" | "!=" | "<" | "<=" | ">" | ">=" ;
rangeTest   = attrRef , BETWEEN , value , AND , value ;   (* inclusive *)
prefixTest  = attrRef , STARTSWITH , strLit ;
existsTest  = EXISTS , "(" , name , ")" ;
typeTest    = TYPEOF , "(" , name , ")" , "=" , typeTag ;

attrRef     = name | sysName ;
name        = letter , { letter | digit | "_" | "." | "-" } ;   (* <= 32 bytes *)
sysName     = "$" , name ;

value       = taggedLit | boolLit | numLit ;   (* numLit: untagged number ⇒ i32 *)
taggedLit   = typeTag , "(" , literalBody , ")" ;
typeTag     = "i32" | "u64" | "u256" | "dec" | "str" | "addr" | "key" | "bytes32" | "bool" ;
boolLit     = TRUE | FALSE ;
strLit      = "'" , { utf8Char | "''" } , "'" ;
```

Precedence, tightest first: `NOT`, `AND`, `OR` (a `BETWEEN`'s own `AND` is part
of the predicate). Reserved words:
`AND OR NOT TRUE FALSE BETWEEN STARTSWITH EXISTS TYPEOF`. Type tags are recognized
positionally (before `(` and as `TYPEOF`'s RHS), not globally reserved. `bytes`
has no literal, since it can never be indexed.

### Operator × type matrix

| | `=` `!=` | `<` `<=` `>` `>=` `BETWEEN` | `STARTSWITH` |
|---|---|---|---|
| `bool` `i32` `u64` `u256` `dec` `bytes32` `addr` `key` | ✓ | ✓ | — |
| `str` | ✓ | ✓ | ✓ |
| `bytes` | — | — | — |

Range comparisons and `BETWEEN` follow the index order: numeric for `i32` `u64` `u256` `dec`,
`false < true` for `bool`, and unsigned byte order for `str` (raw UTF-8, no
collation), `bytes32`, `addr` and `key`. `STARTSWITH` on a type other than `str`
is a **parse error**, not an empty result.

### Attribute names

`[A-Za-z][A-Za-z0-9_.\-]*`, ≤ 32 bytes, for indexed and unindexed attributes
alike.

- **Case-sensitive.** `Level` ≠ `level`.
- `$` reserved for system attributes; `#` reserved for the structural key cell.
- Reserved keywords (`AND OR NOT TRUE FALSE BETWEEN STARTSWITH EXISTS TYPEOF`) and type
  tags (`i32 u64 u256 dec str addr key bytes32 bool`) are not allowed as names.

### System attributes

Every entity carries `$key`, `$owner`, `$creator`, `$createdAt`, `$updatedAt`,
`$expiresAt` and `$creationFlags`. The engine alone writes them; a user
attribute or mutation naming one reverts (§6).

Queryable:

| name | type | ops |
|---|---|---|
| `$key` | `key` | all comparisons |
| `$owner`, `$creator` | `addr` | all comparisons |
| `$expiresAt` | `u64` (block height) | all comparisons |

Returned in results only, not queryable: `$createdAt` (`u64`, block height),
`$updatedAt` (`u64`, block height), `$creationFlags`.

## 5. Read path — JSON-RPC methods

### `arkiv_query`

JSON-RPC 2.0, positional params `[queryString, options?]`. Chain quantities are
`0x` hex; attribute values follow the JSON representation in §2. Query budgets
(below) are node-local DoS protection, not consensus metering.

**Request**

```json
{
  "jsonrpc": "2.0", "id": 1,
  "method": "arkiv_query",
  "params": [
    "level >= i32(10) AND score >= dec(3.5) AND parent = key(0x123…)",
    {
      "atBlock": "0x8e1ff",
      "select": {
        "key": true, "owner": true, "expiresAt": true,
        "attributes": { "projectId": true, "version": true, "body": true }
      },
      "limit": "0x64",
      "cursor": "b64:…"
    }
  ]
}
```

| field | type | default | rules |
|---|---|---|---|
| `atBlock` | hex quantity | head | must be within the node's retained range |
| `select` | object | `{ "key": true }` | everything except `key` defaults **off** |
| `limit` | hex quantity | node default | node max 200 |
| `cursor` | string | — | opaque, from a previous response; bound to query/block/select |

**Projections** (`select`) — flat, per-field booleans:

| key | returns |
|---|---|
| `key` | entity key |
| `owner`, `creator`, `createdAt`, `updatedAt`, `expiresAt`, `creationFlags` | individual system attributes |
| `attributeSchema` | user attribute names, types and indexed flags, **no values** |
| `attributes` | names + types + values, indexed and unindexed alike: `true` = all, `{name: true}` = named subset |
| `count` | **aggregate**, not per-entity: total number of entities matching the query at `atBlock`, returned as top-level `result.count` |

Selecting is independent of indexing: an unindexed attribute cannot appear in
the query string but can be selected and returned.

**`count`** — the total size of the match set, **independent of `limit` and
`cursor`**. It is a hex QUANTITY, repeated on every page it is selected on, and
stable across pages (the cursor pins `atBlock`). If `count` is the **only**
selected field, `data` is `[]` and no `cursor` is returned. `"*"` +
`{ "count": true }` counts all active entities.

**Response**

```json
{
  "result": {
    "blockNumber": "0x8e1ff",
    "cursor": "b64:…",
    "count": "0x2a",
    "data": [
      {
        "key": "0x123…",
        "owner": "0xabc…",
        "creator": "0xabc…",
        "createdAt": "0x191",
        "updatedAt": "0x1bb",
        "expiresAt": "0x92e21",
        "creationFlags": { "readonly": false, "permissionlessExtension": true, "raw": 2 },
        "attributeSchema": [
          { "name": "projectId", "type": "str", "indexed": true },
          { "name": "version", "type": "i32", "indexed": true },
          { "name": "body", "type": "bytes", "indexed": false }
        ],
        "attributes": [
          { "name": "projectId", "type": "str", "value": "alice-123" },
          { "name": "version", "type": "i32", "value": 4 },
          { "name": "score", "type": "dec", "value": "3.5" },
          { "name": "balance", "type": "u256", "value": "0xf4240" },
          { "name": "body", "type": "bytes", "value": "0x7b22…" }
        ]
      }
    ]
  }
}
```

- `cursor` omitted when no pages remain.
- `count` present only when selected.
- `type` is always the bare tag; the indexed flag is reported by
  `attributeSchema`.
- **Result ordering (open):** cursor pagination needs a deterministic total
  order across pages; the ordering key is not yet pinned.

### Account & nonce reads

- `eth_getBalance(addr)`, `eth_getTransactionCount(addr)` — standard `eth_*`,
  served from Golem DB account state.
- `entityNonce(owner)` — the per-owner entity-nonce counter used in key
  derivation (§3); `txNonce(owner)` — the account tx nonce. *(Exact method
  names: open.)*
- `eth_getProof` — membership/account proofs against the block `state_root`.

**Open — proof serving.** No method yet serves the store-side leg of a record
membership proof (from `data_root` to the record), or index completeness proofs.

### `eth_*` namespace

Tx submission (`eth_sendRawTransaction`), receipts, logs, blocks and
`eth_getProof` behave as in Ethereum; account reads are as above. There is no
contract-deployment or bytecode-execution surface.

## 6. Errors

**Query RPC errors** (`arkiv_query`) — stable codes + machine-readable `data`:

| code | error | example `data` |
|---|---|---|
| −32001 | parse error | `{ "position": 14, "expected": "typed literal", "got": "3.5" }` |
| −32002 | type error | `STARTSWITH` on a non-`str` type; `BETWEEN` bounds of different types; unknown tag |
| −32003 | literal validation | i32 out of range; bad EIP-55 checksum; > 18 dp |
| −32004 | limits exceeded | query length / predicate count / nesting / bitmap-op budget |
| −32005 | cursor error | expired, malformed, or bound to a different query/block/select |
| −32006 | block unavailable | `atBlock` outside retained range |

**Write/ABI typed reverts** (`execute`) — a transaction reverts (atomically) on
any of: empty batch; unknown op tag; non-canonical `operationData`; tombstone in
`create`; unknown type in the low seven `typeId` bits; `bytes` with the indexed
flag set; a tombstone with any `typeId` other than `0x00`; a `bytes` value over
128 KiB or an entity over the per-entity size cap; an engine-controlled `$` cell
in a user attribute/mutation list; name grammar violation; expiry that would
shorten (`extend_expiry`), is dead-on-arrival (`target ≤ current`), or breaks
the lifetime domain (`expiresAt > MAX_EXPIRES_AT`, `minLifetime > MAX_LIFETIME`,
or a finite `target > current + MAX_LIFETIME`); transfer to zero/self; op auth
failure (owner-only).