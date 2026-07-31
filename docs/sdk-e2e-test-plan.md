# SDK e2e test plan for the typed attribute system

The tests an SDK's CI should run against a live `arkiv-node` (the GHCR image,
`--dev` mode, or the kurtosis devnet) to verify the type system across the ABI
and RPC boundary. This is the *target* surface from the Typed Query Language
spec; each group is marked with what works on today's `main` so SDK work can
start on the green parts immediately.

Legend: **[green]** — passes against `main` today. **[partial]** — works with
divergences noted. **[blocked]** — needs harness work first (see
"Query-side gaps" in the audit; mostly the typed-literal parser and the RPC
option/error surface).

## A. Write path: one attribute of every type round-trips [green]

For each user-settable type, create an entity with one attribute of that type,
then read it back via `arkiv_getEntity` and assert name, typeId, and value are
byte-identical:

| type | boundary values to cover |
|---|---|
| `bool` (1) | `true`, `false` |
| `i32` (2) | `0`, `-1`, `i32::MIN`, `i32::MAX` |
| `u256` (3) | `0`, `1`, `2^256 − 1` |
| `dec` (4) | `0`, negative, exactly 18 fractional digits, integral |
| `bytes32` (5) | all-zero, all-FF |
| `str` (7) | empty, multibyte UTF-8, exactly 128 bytes |
| `addr` (8) | zero address, mixed-value address |
| `key` (9) | a real minted key, a dangling reference (must be accepted) |

Also: an entity carrying all 8 types at once, and the `MAX_ATTRIBUTES = 32`
boundary (32 accepted, 33 reverts `TooManyAttributes`).

## B. Write-path validation reverts [green, one divergence]

Each of these must revert with the expected typed error, and the entity must
not exist afterwards:

- i32 with bad sign-extension padding (a uint256-shaped word) → `InvalidValueType`
- unknown typeId (0, 10, 255) → `InvalidValueType`
- typeId 6 (`bytes`, system-only) → `InvalidValueType`
- attributes not name-ascending, and exact-duplicate names → `AttributesNotSorted`
- empty attribute name → `Ident32Empty`; name > 32 bytes → structural
- `$`-prefixed name → `Ident32InvalidByte` (system namespace is unreachable)
- create with zero BTL → revert
- **Divergence to settle first:** node currently accepts only
  `[a-z][a-z0-9._-]*` names — the spec says `[A-Za-z]…` and case-sensitive.
  Test whichever way the decision lands; today uppercase reverts.
- **Not currently validated (spec says it should be):** >18-decimal-place
  `dec` is accepted verbatim (scale is client convention); `contentType` is
  not MIME-validated on the write path; reserved words (`and`, `not`, `true`,
  `i32`, `str`, …) are currently *legal* attribute names.

## C. Operation lifecycle visible through queries [green]

- create → matches by its attributes; update → old value stops matching, new
  value starts (reindex), including an update that **changes an attribute's
  type** (old type's predicate no longer matches, new type's does)
- **update is a full replace** — attributes omitted from an update are gone
  (there is no patch/unset op; SDK must model this)
- transfer → entity moves between `$owner` buckets; `$creator` unchanged
- extend → new expiry queryable; extend on expired entity reverts
- delete → gone from every query, `arkiv_getEntity`, and the count
- batch: N creates in one `execute` land at N distinct keys with sequential
  nonces; one failing op reverts the whole batch (atomicity); non-owner
  mutations revert
- expiry: entity past its BTL is hidden from `arkiv_getEntity`, `arkiv_query`,
  and `arkiv_getEntityCount` without any delete op

## D. Query: operator × type matrix

The core of the typed system. For each cell of the spec's matrix, seed
entities and assert exact match sets:

- **[blocked — typed literals]** `=` / `!=` for all 8 types; `< <= > >=` for
  `i32` (must span negatives), `u256` (above 2^64), `dec` (negatives +
  fractional ordering); `STARTSWITH` on `str` (byte prefix, multibyte UTF-8
  edge: prefix that splits a codepoint matches bytewise)
- **[blocked]** typed no-cross-match: `level` stored as `i32(10)` on one
  entity and `u256(10)` on another — `level >= i32(10)` returns only the
  first; `typeof(level) = i32` distinguishes them
- **[blocked]** `exists(attr)` — set with any type; `!=` matches only
  entities that *have* the attribute with that type (not the absent ones —
  this is a semantic change from today's `$all`-complement)
- **[blocked]** range op on an equality-only type (`addr`, `key`, `bytes32`,
  `bool`) and `STARTSWITH` on non-`str` → parse/type error, never empty result
- **[green with old syntax]** boolean structure: `AND`/`OR`/`NOT` precedence,
  parens, `NOT` complement over the live set
- **[partial]** system attributes: `$owner`/`$creator` eq by address,
  `$expiresAt`/`$createdAt` untagged range (today named
  `$expiration`/`$createdAtBlock`), `$contentType` eq + prefix, `$key` eq;
  `$updatedAt` **[blocked — not indexed at all today]**

## E. Projections (`select`) [blocked — today only `includeData` with different names]

- default is `{key: true}` only
- each field individually: `owner`, `creator`, `createdAt`, `updatedAt`,
  `expiresAt`, `contentType` (without payload), `payload`, `creationFlags`
  **[blocked — flags don't exist in the data model yet]**
- `attributeSchema` (names + types, no values) **[blocked]**
- `attributes: true` vs `attributes: {name: true}` subset **[blocked]**
- response value encodings per type: u256 → hex string, i32 → JSON number,
  dec → decimal string, bool → JSON bool, addr/key/bytes32 → fixed-width 0x,
  chain quantities → hex **[blocked — today all values are strings]**

## F. Pagination [partial]

- full cursor walk at small page size: pages partition the match set, no
  duplicates/omissions, cursor omitted on the last page **[green]**
- `limit` above node max → error (today: silent clamp) **[blocked]**
- malformed cursor, and cursor reused with a different query / block /
  select → `-32005` **[blocked — cursor is an unbound transparent id today]**
- pagination stability while new entities are being written

## G. Error taxonomy [blocked — today only -32602/-32603, no data]

One test per code asserting code + machine-readable `data`:
`-32001` parse (with position), `-32002` type error (range op on
equality-only type), `-32003` literal validation (i32 range, EIP-55, >18 dp),
`-32004` limits (query length / predicate count / nesting), `-32005` cursor,
`-32006` block unavailable.

## H. Historical reads [green]

- write at block N, update at N+k: query `atBlock: N` sees the old value,
  head sees the new one
- entity expired at head still visible at a pre-expiry `atBlock`
- future block and unsupported tags rejected

## I. Ambient guarantees worth one smoke test each

- `nonces(owner)` advances per create; predicted key matches minted key
- node restart preserves entities and index (query before == after)
- concurrent writers: two SDK clients batching against one node; no
  cross-contamination of nonces or keys

## Suggested wiring

Run groups A–C and H–I against the released node image now — they pin the ABI
boundary the SDK builds against. Land groups D–G in the same PRs that close
the corresponding harness gaps, so the spec surface and its conformance tests
arrive together. The harness repo's own `bin/arkiv-node/tests/e2e.rs` is the
reference for spawning and driving a dev node.
