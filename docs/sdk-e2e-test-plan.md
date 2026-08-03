# SDK e2e test plan for the typed attribute system

The tests an SDK's CI should run against a live `arkiv-reth` (the GHCR image,
`--dev` mode, or the kurtosis devnet) to verify the type system across the ABI
and RPC boundary.

Status is against the `raz-glm/feat/typed-queries` branch, which implements the
typed query language and the `arkiv_query` wire surface. Legend: **[green]** —
passes today. **[partial]** — works with the divergence noted. **[cut]** — not
in V1 by decision, see `v1-typed-query-scope.md`.

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

- **[green]** `=` for all 8 types; `< <= > >=` for `i32` (must span
  negatives), `u256` (above 2^64), `dec` (negatives + fractional ordering);
  `STARTSWITH` on `str` (byte prefix; multibyte UTF-8 edge: a prefix that
  splits a codepoint matches bytewise)
- **[green]** typed no-cross-match: `level` stored as `i32(10)` on one entity
  and `u256(10)` on another — `level >= i32(10)` returns only the first. This
  falls out of the index addressing, so it needs no runtime check
- **[green]** range op on an equality-only type (`addr`, `key`, `bytes32`,
  `bool`) and `STARTSWITH` on non-`str` → type error (−32002), never an empty
  result
- **[green]** boolean structure: `AND`/`OR`/`NOT` precedence, parens, `NOT`
  as the full complement over the live set
- **[green]** system attributes: `$owner`/`$creator` eq by address (tagged
  `addr(…)` or a quoted hex string), `$expiresAt`/`$createdAt` untagged range,
  `$contentType` eq + `STARTSWITH`, `$key` eq
- **[cut]** `exists(attr)`, `typeof(attr) = T`, and typed `!=`. Write
  `NOT (attr = value)` for the complement; the rest have stop-gaps in the
  scope doc. All three are parse errors, so a query never silently means
  something else
- **[cut]** `$updatedAt` as a *filter* — it is projectable but not indexed

## E. Projections (`select`) [green]

- **[green]** default is `{key: true}` only — assert nothing else comes back
- **[green]** each field individually: `owner`, `creator`, `createdAt`,
  `updatedAt`, `expiresAt`, `contentType` (selectable *without* the payload),
  `payload`
- **[green]** `attributeSchema` (names + types, no values)
- **[green]** `attributes: true` vs `attributes: {name: true}` subset
- **[green]** response value encodings per type: u256 → hex quantity, i32 →
  JSON number, dec → decimal string, bool → JSON bool, addr/key/bytes32 →
  fixed-width 0x, str → string; chain quantities → hex
- **[green]** an unknown `select` field is rejected rather than ignored, so a
  typo'd projection fails loudly instead of returning nothing
- **[cut]** `creationFlags` — entities carry no flags yet; selecting it errors

## F. Pagination [green]

- **[green]** full cursor walk at a small page size: pages partition the match
  set, no duplicates or omissions, cursor omitted on the last page
- **[green]** `limit` above the node max (200), and `limit: 0` → error, not a
  silent clamp
- **[green]** malformed cursor, and a cursor reused with a different query /
  block / select → −32005. Cursors are opaque (`b64:…`) and bound to the
  request that issued them
- pagination stability while new entities are being written

## G. Error taxonomy [green]

One test per code, asserting the code **and** the machine-readable `data`
(parse-side errors carry `position`):

| code | trigger to test |
|---|---|
| −32001 | `rank = = u256(1)`, or a removed operator (`&&`, `~`, `!`) |
| −32002 | `rank != u256(1)`, `team >= str('x')`, `exists(x)`, `$nope = true` |
| −32003 | `i32(2147483648)`, `dec(0.1234567890123456789)`, a bad EIP-55 checksum |
| −32004 | query over 8 KiB, over 64 predicates, or nested over 32 deep |
| −32005 | malformed cursor, or one from a different query/block/select |
| −32006 | `atBlock` ahead of the tip, or a pruned block |

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

Every group except the **[cut]** items is implementable against the node
today, so the whole plan can go into SDK CI in one pass rather than being
staged behind harness work.

Two things worth knowing while writing them:

- `arkiv_getEntity` still answers in an **older shape** than `arkiv_query` —
  plain-number block fields, and attributes as `{key, valueType, value}` with
  the value hex-encoded. Only `arkiv_query` follows the spec encodings. If the
  SDK reads entities through both, expect to normalize; aligning the two is a
  follow-up.
- The harness repo's own `bin/arkiv-reth/tests/e2e.rs` is the reference for
  spawning and driving a dev node, and already covers most of groups D–G — it
  is the closest thing to a conformance suite to crib from.
