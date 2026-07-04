# DB Engine — TODO

*Growing task list for `crates/arkiv-db-engine` (and its executor seam). Each
section follows the same structure: **current state** — what the code does
today, with pointers — then **change proposal** with motivation. Companion
docs: `architecture-review-scope.md` (intended architecture),
`repo-code-structure.md` (code survey).*

**Background information.** The engine stores *entities* —
a payload plus typed attributes plus lifecycle metadata — in reth's
account state. Terms used throughout:

- **attributes** — the typed `(name, value)` pairs on an entity. System
  attributes are `$`-prefixed (`$owner`, `$expiration`, …); user
  attributes are everything else.
- **tier-1 / pair bitmap** — for every *indexed* `(attribute, value)`
  pair, a roaring bitmap of the entity IDs holding that pair: the
  equality and reverse-lookup index. (Not every attribute is indexed —
  `$payload`, for example, has no index.)
- **tier-2** — per indexed attribute, an ordered directory of *which
  values exist* (Int-ordered for numerics, Str-ordered for strings).
  Range and prefix queries scan tier-2 to find matching values, then
  union those values' tier-1 bitmaps.
- **btl** — "blocks to live": the relative lifetime supplied at
  create/extend, stored as an absolute `expires_at`.

References: bare **§N** = a section of this document; **scope-doc §N** =
`architecture-review-scope.md`. Sections are ordered by dependency: data
model (§1–2) → protocol limits (§3) → ports (§4–5) → mechanisms built on
them (§6–9) → economics & verification (§10–11) → process (§12).

---

## Contents

- [1. Attribute types & entity references](#1-attribute-types--entity-references)
- [2. Entity operations — fine-grained op set](#2-entity-operations--fine-grained-op-set)
- [3. Protocol limits](#3-protocol-limits)
- [4. Inbound port — `fn execute` (rename + reshape `dispatch`)](#4-inbound-port--fn-execute-rename--reshape-dispatch)
- [5. Outbound port — `RecordStore` (generic untyped record store)](#5-outbound-port--recordstore-generic-untyped-record-store)
- [6. Entity key creation — salt as primary, nonce as fallback](#6-entity-key-creation--salt-as-primary-nonce-as-fallback)
- [7. Entity storage layout — payload in code, metadata in slots](#7-entity-storage-layout--payload-in-code-metadata-in-slots)
- [8. Index storage layout — segmented bitmaps](#8-index-storage-layout--segmented-bitmaps)
- [9. Index implementation over the record store](#9-index-implementation-over-the-record-store)
- [10. Metering](#10-metering)
- [11. Conformance corpus](#11-conformance-corpus)
- [12. DB engine isolation — own repo, on triggers](#12-db-engine-isolation--own-repo-on-triggers)

---

## 1. Attribute types & entity references

### Current state

Three attribute types (`call.rs` / `lib.rs`): `ATTR_UINT = 1` (one 32-byte
BE word), `ATTR_STRING = 2` (≤ 128 B, UTF-8, no embedded nulls),
`ATTR_ENTITY_KEY = 3` (32 raw bytes). Indexing: uint → tier-1 + Int tier-2;
string → tier-1 + Str tier-2; entity key → **tier-1 only** (skip-listed —
hash values, ordering meaningless; correct call).

- **Entity refs already give both graph primitives**: forward traversal
  (typed attribute → load target) and reverse lookup (the tier-1 pair
  bitmap for `(attr_key, target)` *is* a backlink index) — but reverse
  lookup is per-attribute-key; attribute-agnostic "who references X at
  all" requires knowing every attr name.
- **No signed ints** — and naive two's-complement in the current encoding
  would mis-sort (negatives have the high bit set → lex order ≠ numeric
  order).
- **No address type** — `$owner`/`$creator` are address-valued attributes
  internally (tier-1, skip-listed), but users can't declare address
  attributes; misuse pressure lands on strings (hex + case-mismatch
  equality bugs) or uint (address-as-uint160).
- **References carry no domain**: `chain_id` is in the key-derivation
  preimage (separation) but unrecoverable from the key (no
  identification); every entity-key attribute is implicitly local.
- **Unknown `valueType` is a landmine**: `convert_attributes` returns
  `ApplyError::Fatal` (execution-killing internal error) instead of a
  typed revert (`call.rs:585`).
- Links are de-facto weak references (no existence check at write, no
  delete restriction) — true but nowhere stated as a decision.

### Change proposal

**V1 type set — six types** (types added before genesis are free; after
genesis every type is a hard fork). The engine-type column names the Rust
types used in the sections below — all from `alloy-primitives`, the
byte-primitive layer reth itself builds on (host-neutral: fixed-width
bytes + hashing, no node/EVM semantics):

| tag | type | value encoding | engine type | indexing | notes |
|---|---|---|---|---|---|
| 1 | `uint` | 32 B BE word | `U256` | tier-1 + Int tier-2 | unchanged |
| 2 | `string` | ≤ 128 B UTF-8 | `Vec<u8>` (UTF-8 validated) | tier-1 + Str tier-2 | unchanged |
| 3 | `entity key` (local) | 20 B (§6) | `EntityKey` — newtype over `[u8; 20]` | tier-1 only | a **relative** ref ("the entity in *this* state"). Deliberately NOT alloy `Address`: same width, distinct Rust type — the key/address confusability (§6 cons) is kept out of the type system |
| 4 | `address` | 20 B, ABI right-aligned in word 0 | `Address` | tier-1 only | **new** — "principal reference" (neutral term; host binds Ethereum account); same encoding/keyspace as `$owner`/`$creator` |
| 5 | `int` (signed) | 32 B, **sign-bit-biased BE** (`value XOR 0x80…00`) | `I256` (bias applied at encode) | tier-1 + Int tier-2 | **new** — bias makes lex order = numeric order across the signed range; distinct tag (orderings incompatible with uint); parser gains signed literals |
| 6 | `remote entity key` | `domain_be8 ‖ key20` (28 B) | `(u64, EntityKey)` | tier-1 only | **reserved, deferred** — see below |
| 7 | `bytes32` | 32 B opaque | `B256` | tier-1 only | **new** — honest home for hashes / commitments / external IDs; no tier-2 (values effectively random); `$entityHash` (§7) is this type. Prevents hex-in-string misuse and abuse of entity-key for non-key hashes (which stops even fitting once keys are 20 B) |

Entity keys are 20 bytes (§6); tags 3 and 6 carry that width. Alignment
convention inside the `bytes32[4]` ABI container must be pinned per type:
entity keys left-aligned (hash prefix), addresses right-aligned (ABI
convention), `bytes32` full-word.

**Reference taxonomy** (spec language): attributes reference three target
spaces — (i) *internal*: local entity keys, into this state `S`,
existence checkable in principle; (ii) *external state*: remote entity
keys, another domain's `S`, unverifiable by construction; (iii)
*identity*: principals/addresses, not part of `S` at all — nothing to
resolve, weak by nature. Addresses need no remote variant (identity is
domain-invariant; per-chain account *state* is never modeled).

**Weak references are the explicit spec decision**: no existence check at
write time, no delete restriction on referenced entities, dangling links
permitted. (Stricter policies remain implementable later — "is X
referenced via attr k" is a bitmap non-emptiness check.)

**Remote entity key (tag 6) — designed now, shipped in the multi-chain
fork.** Rationale: the scaling model (per-chain capacity bounded at a few
TB for permissionless node operation → scale-out = more db chains) makes
cross-chain refs the addressing layer of growth, not a speculative
feature. But fork cost is per *version boundary*, not per change — so
batch it into the multi-chain fork rather than spending a boundary early.
**Hard sequencing rule: the type is a launch prerequisite of chain #2,
never a follow-up** — the string-misuse window only opens if a second
chain exists without it. Design pinned now:

- Encoding `domain_be8 ‖ entity_key`; tier-1 only.
- **Canonicalization rule (load-bearing):** reject remote refs with
  `domain == env.domain` ("use the local type") — otherwise one target
  has two encodings and its backlink bitmap fragments.
- Type vs. ecosystem split: the tag/validation ship with the fork;
  domain-id registry semantics, SDK multi-chain resolution, cross-chain
  indexers are separate, later workstreams.

**Sharding implications to state in the spec** (follow from the key
derivation including `domain`):

- **Entities are birth-chain-bound**: same logical object re-created on
  another shard gets a different key, by design. Sharding = *placing*
  data, never moving it. App-level identity spans shards (synergy: §6
  salt mode makes one logical ID's per-shard keys predictable —
  `salt = hash(logical_id)` on every shard).
- **Cross-chain reverse lookup is asymmetric by construction**: "who on
  this chain references remote X" = one local bitmap read; "who
  references my entity from other chains" is unanswerable locally —
  indexer territory. Sharded apps must design layouts around the cheap
  direction.

**Hygiene items (V1, independent of the deferral):**

1. Unknown attribute tag → typed revert `InvalidAttributeType` (replaces
   `ApplyError::Fatal`) — pins the pre-fork behavior of all future types
   into V1 rules; failures become clean receipts instead of
   execution-killing errors.
2. Tag discipline: additive only, never reinterpret an existing tag;
   tag 6 reserved for remote entity key.
3. Design the **rule-version activation mechanism** (δ version selected
   by height, chainspec-style) before the first fork needs it —
   re-execution verification must apply the rules active at each height,
   so versioned δ should be a designed feature, not an emergency patch.
   Fork policy: batch changes into few, named forks (cost is per
   boundary, not per change); pre-genesis changes are free.

**Open (decide when implementing):** the `$linkedBy` aggregate built-in —
an auto-maintained `("$linkedBy", target_key)` attribute on every
entity-key attribute write, giving attribute-agnostic backlinks for one
extra bitmap update per link write; alternative is leaving aggregation to
off-chain tooling. Explorer value is high; decide with the tooling team.

---

## 2. Entity operations — fine-grained op set

### Current state

Six operation types (`call.rs`, tags 1–6): `create`, `update`, `extend`,
`transfer`, `delete`, `expire`. Four of them are already field-granular
(`extend` touches expiry, `transfer` ownership, `delete`/`expire`
lifecycle). The problem is **`update`: it replaces payload + content type
+ the full attribute set wholesale.**

- The engine diffs attributes internally (`apply_annotation_diff` touches
  only changed bitmaps) — but the **client must transmit the entire new
  entity state**: full payload bytes even if unchanged, the complete
  attribute list to change one attribute.
- Consequences: calldata is O(entity) for any change; every update forces
  a client-side **read-modify-write cycle** (fetch entity → modify →
  resend), which creates **lost-update races** — two writers changing
  *different* attributes clobber each other's changes; metering (§10)
  would have to charge O(entity) for semantically one-field changes; and
  the §7 storage layout can write field-granular, but the op surface
  can't express it.

### Change proposal

**Lifecycle ops stay dedicated; all content mutation collapses into one
generic `set` op.** The dividing rule: an op is dedicated iff it carries
distinct authorization/validation semantics; pure content changes share
one uniform patch surface.

| op | signature (conceptual) | semantics |
|---|---|---|
| `create` | `(salt?, btl, entries[])` | mint key (§6 salt/auto) + apply `entries` — same entry shape as `set` |
| `set` | `(key, entries[])` | partial patch: apply entries in order; owner-only |
| `extend` | `(key, btl)` | dedicated: expiry monotonicity rule. **Anchored at now, not additive** — `new_expires_at = current_block + btl` ("lifetime ≥ btl from now", TTL-refresh semantics), NOT `expires_at + btl`. Intentional: retry-idempotent (a replayed extend can't double-extend), concurrent renewers converge instead of stacking, and prepayment horizon stays bounded at `now + btl_max` (no locking in today's storage prices for stacked centuries). Additive behavior is client-emulable (`btl = expires_at − now + n`); the reverse is not — the anchored form is the primitive. Naming alternatives (`refresh`, `renew`, `touch`) considered and rejected; the name stays `extend` despite its additive flavor — this row is the canonical semantics |
| `transfer` | `(key, newOwner)` | dedicated: zero/self checks, ownership authorization |
| `delete` | `(key)` | dedicated: removal; auth = owner **OR entity expired** (see below) |

**The `set` entry model** — an ordered list of `(name, valueType, value)`
triples over one unified keyspace:

- **Keys**: system namespace (`$`-prefixed: `$payload`, `$contentType`)
  or user namespace (must NOT start with `$` — already enforced by the
  ident charset, which excludes `$` from user attribute names). Entity
  content becomes one addressable map; metadata and attributes stop
  being structurally different things at the op surface.
- **`NULL` value = removal** of the key (distinct from empty: encode the
  tombstone as `valueType = 0`, reserved — so "remove" and "set to empty
  value" are unambiguous). **Removal is by name alone** — the engine
  knows the stored type and cleans the right indexes. `$payload = NULL`
  clears the payload (legal: §7 existence does not depend on payload).
- **Canonical order: entries strictly ascending by name, duplicates
  rejected.** Buys three things at once: a canonical input encoding,
  duplicate detection in a single validation pass, and no
  last-write-wins ambiguity inside one op.
- **System-key writability table (spec item)**: settable via `set` =
  `$payload`, `$contentType`; lifecycle-op-only = `$owner` (transfer),
  `$expiration` (extend); immutable = `$creator`, `$createdAtBlock`,
  `$key`; derived, never client-writable = `$entityHash`,
  `payload_hash`. `set` on a non-settable system key → typed revert.
- `create` uses the same entry shape for its initial content — one
  encoder/validator for both ops.

**User attribute typing — decided: `name → (type, value)` (unique names
per entity), MongoDB-style query semantics.**

- **Explicit type tag on every entry** (`valueType: u8`, §1 taxonomy).
  Self-describing writes, local validation, no registry lookups —
  and inference from value bytes is impossible anyway (address and
  entity key are both 20 B; only the tag disambiguates).
- **Per entity, names are unique**: an entity is a map
  `name → (type, value)` — the natural record model for SDK structs and
  explorers; it never holds `x:uint` and `x:string` simultaneously.
  A `set` entry with a different type than stored is a **type change**
  = remove old triple from its old-type indexes, insert new — no
  special case (and why the typeless `valueType = 0` tombstone works).
  The alternative — `(name, type)` as identity, with coexisting typed
  twins like `version:uint` and `version:string` on one entity — was
  considered and rejected: unique names plus the industry-standard
  semantics below are the easier sell, and the alternative's main
  benefit (parse-time type errors) is mostly recovered via typed
  literals.
- **The stored type still joins the index keyspace**: pair-bitmap key =
  `name ‖ type ‖ value`. This fixes a latent bug (today `pair_address`
  omits the type, so identical value bytes under different types pollute
  one bitmap) and makes the tier-2 index shape (Int-ordered /
  Str-ordered / none) definitional per type rather than heuristic.
- **Query semantics = the schemaless industry standard (MongoDB
  precedent)**: a typed predicate matches only entities whose attribute
  holds that name *at that type* — type mismatch is **no match, not an
  error**. Familiar to every developer who has used a document store;
  no global type registry, no coordination between apps sharing a name.
- **Typed literals: inference for the unambiguous, cast constructors for
  the rest.** Casts type the literal at parse time (`uint(5)`,
  `int(-5)`, `string("x")`, `bytes32(0x…)`, `address(0x…)`,
  `entityKey(0x…)`) — typed literal constructors, never runtime
  conversions. Inference rules (pinned, consensus-adjacent):
  unsigned decimal → `uint`; negative decimal → `int`; quoted →
  `string`; 32-byte hex → `bytes32`; **20-byte hex → parse error, cast
  required** (`address` vs `entityKey` undecidable). Comparator validity
  is checked at parse time against the literal's type (glob only on
  `string`, ranges only on `uint`/`int`/`string`, equality-only for the
  reference types, nothing on `bytes`).
  Documented footgun + SDK norm: bare `5` targets `uint` and misses
  `int`-typed attributes — write `int(5)`; SDKs always emit casts, bare
  literals are human shorthand for consoles/explorers.
- **System types**: system keys have fixed types (`$payload` = unbounded
  `bytes`, `$contentType` = bytes ≤ 128 B). **`bytes` is system-only**:
  it has a tag, but validation rejects it for any non-`$` name —
  `$payload` is the sole entry whose value escapes the 128 B attribute
  bound, so the entry encoding uses variable-length values.

**`expire` is dropped — resolved: expiry only changes access rules,
never performs work.** Reaping folds into `delete` with the auth rule
**"caller == owner OR entity is expired"**: anyone may delete an expired
entity, so bots garbage-collect (V1: operator runs the GC bot; they find
targets via `$expiration ≤ now` range queries with
`QueryEnv.include_expired`, §4).

**Primary reason — expiry bombs.** `btl` lets an attacker aim an
arbitrary number of paid-once creates at one single expiry block.
Protocol-side auto-reaping would owe all of that removal work at that
block — overwhelming block capacity unless the protocol adds a per-block
reap cap plus a backlog queue, i.e. *more* consensus machinery and state
just so the feature survives its own attack surface. With access-rule
expiry, the moment of expiry costs **zero work by construction**: it
flips a predicate (read-side visibility, §4; delete authorization here),
and removal work only ever happens inside paid transactions, bounded by
block capacity like all other work. Formal-model phrasing: **expiry is a
predicate over (entity, time), not an event** — state changes only via
transactions.

Secondary reasons against protocol-side reaping: blocks would mutate
state without user transactions (δ grows a per-block implicit step that
the formal model, re-execution, and conformance corpus must all
reproduce) and the inbound port would need a maintenance entry point.
Semantic safety of deferred reaping comes free from §4's active-view
filtering — an expired-but-unreaped entity is already invisible by
default — so reaping is pure storage hygiene, paid by the deleter.

**GC funding — TO BE DECIDED.** Permission is settled (above); the
incentive is not. Direction: **GC costs are collected up front, at
entity creation/update time** (the entity prepays its own eventual
removal). Open: whether cleanup is performed by off-chain bots run by
the protocol owners (prepaid amount covers operator cost), and/or
whether a financial incentive is offered to whoever performs the
expired-delete (bounty paid out from the prepaid amount — permissionless
GC that actually self-sustains). Interacts with §3 limits: per-entity
delete cost is attacker-shaped (O(#attributes)), so the prepaid amount
must cover the worst case the limits permit.

**Cross-cutting decisions (op set + entry model):**

- **Cost ∝ change.** A `set` of k entries costs k attribute-area/bitmap
  touches (+ blob write iff `$payload` is among them); no O(entity)
  floor on any mutation. This op surface and the §7 slot layout are two
  halves of the same cost model — either without the other strands the
  benefit.
- **Lost-update races disappear structurally.** Partial `set`s compose:
  concurrent writers touching disjoint keys both land. Full-replace
  `update` made every concurrent write a conflict.
- **One mutation path.** A single `set` codepath (validate → diff →
  apply per entry) replaces the wholesale `update` without sprouting a
  per-field op family (`setPayload`, `setContentType`, …) — less
  conformance surface, one event shape (`kind = Set`), one encoder.
- **Extensible without new op tags**: future settable system keys are
  new rows in the writability table, not new operations (still a
  consensus change when added — same fork economics, smaller surface).
- **Op-tag discipline mirrors §1 attribute-tag hygiene**: additive
  numbering, never reinterpret, unknown op tag stays a typed revert
  (`InvalidOpType` exists today — keep), post-genesis additions are
  version boundaries (batch into named forks).
- Batch semantics unchanged: atomic, fail-fast, events in emission order
  (§4). Every mutating op updates `last_modified` and re-derives
  `$entityHash` (§7, cheap by construction).
- ABI impact: the `Operation` struct/encoding changes (pre-genesis free);
  full-replace `update` is **dropped** (SDK emulates via `set`; two write
  paths to the same state is conformance surface for nothing). SDK gains
  a natural patch API — no more fetch-merge-resend to change one field.

---

## 3. Protocol limits

*TODO — section reserved, content to be worked out.*

Candidate contents: max payload size, max entries per `set`, max
attributes per entity, max ops per batch, index-entry cap (§5's 128 B),
query result-set caps — and how each limit feeds the §10 cost schedule.
Note: several existing designs implicitly assume these numbers (the §7
attribute area, per-entity delete cost, §2's "unbounded" `$payload`).

---

## 4. Inbound port — `fn execute` (rename + reshape `dispatch`)

### Current state

The de-facto inbound port is `dispatch` (`call.rs:99`):

```rust
pub fn dispatch<S: StateAdapter>(
    state: &mut S,
    ctx: &CallContext,        // { caller, chain_id, block_number } — NO gas budget
    calldata: &[u8],
) -> Result<CallResult>
```

- `state: &mut S` — already right: the engine reads and writes exclusively
  through the outbound `StateAdapter` port.
- `CallContext` carries caller + chain_id + block_number, but **no gas
  budget** — metering is impossible through the current port.
- `calldata` — right shape (opaque bytes; engine owns decode + validation),
  blockchain-flavored name.
- `CallResult` — fails most of the result-side requirements:
  no gas-consumed field at all; failure reason is a pre-ABI-encoded opaque
  `Bytes` (Solidity revert payload baked into the port); logs are
  `alloy_primitives::Log` (Ethereum address + topics baked into the port).
- The `Result` wrapper's `Err` channel is (correctly) reserved for
  non-deterministic infrastructure failures, but the distinction is
  undocumented.

### Change proposal

Rename to `execute` and reshape both sides of the signature:

```rust
pub fn execute<S: RecordStore>(     // full outbound port (§5): read + write
    state: &mut S,
    env: &ExecutionEnv,
    input: &[u8],
) -> Result<ExecutionResult, EngineFault>;

pub struct ExecutionEnv {
    pub caller: Address,      // authenticated by the host; engine trusts it
    pub budget: u64,          // work allowance, in engine cost units;
                              // host binds tx gas limit − host intrinsic
    pub time: u64,            // discrete monotone clock; host binds block_height
    pub domain: u64,          // key-derivation domain separator; host binds chain_id
}

pub struct ExecutionResult {
    pub cost: u64,            // metered work consumed; present in success
                              // AND failure; invariant: cost <= budget
    pub outcome: Outcome,
}

pub enum Outcome {
    /// Events in EMISSION ORDER (op i in the batch produced event i) —
    /// ordering is consensus-relevant (receipt logs) and is how clients
    /// attribute results to the operations they sent. Never sorted.
    Success { events: Vec<Event> },
    Failure(Failure),         // host discards state diff, still charges cost
}

pub enum Event {              // host-neutral domain events
    Operation { entity_key: EntityKey /* 20 B, §6 */, kind: OpKind, owner: Address, expires_at: u64 },
}

pub enum Failure {            // closed, typed set of deterministic failures
    BudgetExhausted,
    InvalidInput { .. },
    EmptyBatch,
    EntityNotFound { .. },    // entity refs herein: EntityKey (20 B, §6)
    NotOwner { .. },
    // ... one variant per current sol! error
}
impl Failure { pub fn to_revert_bytes(&self) -> Vec<u8>; }  // canonical ABI encoding
```

Key decisions and motivation:

- **`budget` enters via `ExecutionEnv`** — the enabler for metering
  (scope-doc §5: running meter, abort on exhaustion →
  `Failure::BudgetExhausted`). Division of labor: host charges the
  intrinsic/byte floor and passes the remainder as the budget; engine owns
  state-dependent pricing. Invariant `cost <= budget`.
- **`budget`/`cost`, not `gas_limit`/`gas_used`** — the port stays neutral
  enough to live outside a blockchain host, and the naming aligns with the
  formal model's own vocabulary: resource accounting `β_ω` is defined as
  deterministic per-operation *cost*. The unit is an engine-defined,
  consensus-critical **cost unit**; the Ethereum host binds it 1:1 to gas
  (`budget := gas_limit − intrinsic`, receipt `gas_used := cost +
  intrinsic`). A future host can rescale without touching the engine.
- **`cost` on the result, not inside the variants** — failed work is
  paid work; a shape where cost exists only on success invites
  "revert = free" DoS bugs.
- **`time` abstracts block height.** The engine's lifecycle semantics need
  only a discrete, monotonically increasing clock (expiry comparisons,
  `expires_at = time + btl`, last-modified stamps) — nothing about it is
  blockchain-specific. The Ethereum host binds `time := block_height`;
  tests and other hosts bind any monotone counter. Monotonicity
  (non-decreasing across executions in commit order) is a **host
  obligation** — state it as a port precondition.
- **`domain` replaces `chain_id`.** Same value, host-neutral name: the
  field's only job is domain separation in entity-key derivation, and
  "domain" is the standard term for that. (`env_id` considered; rejected
  for the `env.env_id` stutter against `ExecutionEnv` and because it
  suggests configuration identity rather than key-space separation.)
  The Ethereum host binds `domain := chain_id`.
- **No account nonce in the env.** Replay protection is host territory
  (validated/bumped before the engine runs); the engine has its own minting
  counter. Adding it would couple engine logic to host replay mechanics.
  Revisit only if a concrete validation rule needs it.
- **`input` replaces `calldata`** — neutral, data-specific, no collision
  with the entity `payload` attribute. (Rejected: `payload` for exactly
  that collision.) With the read/write split below, `input` carries
  encoded operation batches (§2) only.
- **No `output` on the write path.** A transaction's return data is
  invisible to its submitter on Ethereum (only simulation sees it;
  receipts carry status/gas/logs) — so a write-path output channel is
  structurally dead weight. Events are the only durable result channel;
  anything a client needs back from a write (e.g. minted entity keys)
  belongs in `Event`, not in return bytes. The former `output` field
  existed solely because the `nonces` view shared the port — resolved by
  the split below.
- **Failures and events become types; encodings become functions.**
  `Failure` is a typed enum with `to_revert_bytes()` producing byte-identical
  Solidity revert payloads (SDK-Stability preserved); `Event::Operation`
  states the domain fact, and the Ethereum host adapter renders it as
  today's exact `EntityOperation` log/topic layout — the engine-side name
  drops the `Entity` prefix (redundant inside the entity engine), while the
  SDK-visible wire event keeps its name unchanged. Non-Ethereum hosts and
  the test harness consume the typed forms directly.
- **Two result channels, documented as consensus-critical:**
  `Ok(report)` = every deterministic outcome incl. user failures (receipts);
  `Err(EngineFault)` = infrastructure/invariant failure only — abort block
  processing, never emit a receipt. Collapsing them would let
  non-deterministic I/O errors leak into consensus.

**Decided: single write port, distinct typed read functions.** Reads do not
flow through `execute`; the engine exposes one read function per concern:

```rust
// Both exist today in some form — this formalizes them as ports.
// Bound on RecordRead (outbound port §5, read side): compile-time
// guarantee that reads cannot mutate.
pub fn read_nonce<S: RecordRead>(state: &mut S, owner: Address) -> Result<u32>;
pub fn query<S: RecordRead>(state: &mut S, env: &QueryEnv, query: &str, page: PageParams)
    -> Result<Page>;
// future read ports as needed (e.g. entity-by-key), not yet specified

pub struct QueryEnv {
    /// Evaluation time: the query answers over the ACTIVE view at `time`,
    /// i.e. entities with `expires_at > time`. Same clock as
    /// `ExecutionEnv.time` (host binds block_height).
    pub time: u64,
    /// Default FALSE: expired-but-unreaped entities are invisible.
    /// TRUE widens the view to include them — required by GC bots
    /// (a `$expiration ≤ now` range query must be able to FIND its
    /// targets, §2 delete-auth reaping) and archival tooling.
    pub include_expired: bool,
}
```

Two notions of time, deliberately separated:

- **Snapshot selection** (*which committed state to read*) is host
  territory — decided at read-adapter construction, invisible to the
  engine (see §5 contract point on snapshot consistency).
- **Evaluation time** (*liveness judgment*: active view = `expires_at >
  time`) is a semantic parameter of the query and crosses the port via
  `QueryEnv`. Canonical binding: `time := the snapshot block's time`, so
  answers match what re-execution would verify; a host passing any other
  `time` is asking a hypothetical ("what is/was/will be live at t") — 
  permitted, but not the verifiable default.
- **Known gap in current code:** the interpreter has no time input and
  never filters by expiry — entities past `expires_at` but not yet reaped
  by an `Expire` op ARE returned by queries today (`resolve_id` checks
  only for code presence). That violates the formal model's active-view
  definition independent of this redesign; fix lands together with
  `QueryEnv`.

Further notes on the read/write split:

- `read_nonce` already exists (`lib.rs`); `query` exists as
  `query::execute` (`query/interpreter.rs`). The change is declaring them
  the read-side port surface rather than internal helpers.
- **ABI routing becomes an adapter concern, not a port concern.** Today's
  `call.rs` selector dispatch survives as an engine-side *wire adapter* for
  Ethereum-shaped hosts (SDK-Stability: `eth_call` of `nonces(address)`
  keeps working byte-identically — the adapter decodes the selector, calls
  `read_nonce`, ABI-encodes the return). Non-Ethereum hosts and the test
  harness call the typed read functions directly.
- The outbound-port side of this split is `RecordRead` /
  `RecordStore: RecordRead` — see §5. Read-side adapters (e.g. over a
  historical `StateProvider` for the watcher RPC) then implement only
  `RecordRead`; also resolves the current wart where
  `ExecutorStateAdapter::iter_storage_asc` bails on the write path.
- Read metering is out of scope here (reads are host/RPC territory for
  now) — but the unbounded-read-cost observation from the code survey
  (`Not`/glob queries materialising the full value universe) transfers to
  whoever serves these ports; revisit when the watcher RPC lands.

---

## 5. Outbound port — `RecordStore` (generic untyped record store)

### Current state

The outbound port is the `StateAdapter` trait (`lib.rs` ~582): `code` /
`set_code` / `storage` / `set_storage` / `tombstone_code` /
`ensure_account_persists` / `iter_storage_asc` over `Address` + `B256` —
i.e. **the Ethereum account model is the port**. Consequences:

- The core hard-codes host-state semantics: keccak-derived account
  addresses for every domain object (entities, pairs, index nodes, system
  slots), RLP-as-account-code, the `0xFE` code prefix, EIP-161 pruning
  workarounds (`ensure_account_persists`, tombstone with nonce=1).
- ~800 lines of the hairiest consensus-critical code — the in-storage
  B+ tree, the 4-level string cascade, the enumeration lists — exist only
  because the MPT keyspace is non-enumerable: ordered iteration emulated
  out of point reads. Substrate pathology living in the core.
- `iter_storage_asc` bails in the production write-path adapter; reads are
  not separated from writes at the trait level.
- Porting to another substrate (in-memory test host, hybrid/custom state
  options b/c) means rewriting core logic, not swapping an adapter.

### Change proposal

Replace `StateAdapter` with a **generic, untyped record store** — the engine
encodes its typed values into `Word`/`Bytes`; the port and adapters never see
an Arkiv type:

```rust
// outbound (driven) port — a generic, UNTYPED record store, two traits:
// RecordRead is the narrow read-only view; RecordStore extends it with
// writes (supertrait: every RecordStore is also a RecordRead).

// host functions — pure, deterministic, fixed per deployment;
// supertrait of RecordRead so one bound carries both capabilities
trait Host {
    fn hash(&self, data: &[u8]) -> B256;   // bytes32 digest; Ethereum host: keccak256
}

// read side — what the read ports (§4 query / read_nonce) are bound on
trait RecordRead: Host {
    fn has_record(&mut self, r: RecordKey) -> Result<bool>;
    fn cell(&mut self, r: RecordKey, c: CellKey) -> Result<Word>;   // zero word = unset
    fn blob(&mut self, r: RecordKey) -> Result<Bytes>;              // empty = unset
}

// full store (read + write) — what `execute` (§4) is bound on
trait RecordStore: RecordRead {
    fn create_record(&mut self, r: RecordKey) -> Result<()>;
    fn delete_record(&mut self, r: RecordKey) -> Result<()>;

    // cell lane — fixed-width KV within a record
    fn set_cell(&mut self, r: RecordKey, c: CellKey, v: Word) -> Result<()>;  // zero clears

    // blob lane — one variable-length value per record
    fn set_blob(&mut self, r: RecordKey, b: Bytes) -> Result<()>;             // empty clears
}
// Word = fixed bytes (e.g. [u8; 32]); Bytes = arbitrary length.
// Seven point operations, NO iteration primitive — indexes are
// engine-composed over these two lanes (§9).
```

**Mapping table — current implementation → RecordStore.** Everything the
engine stores today fits the two lanes (rows 7–11 as engine-composed
structures, §9); the last rows are host mechanics that stop crossing the
port at all.

| # | Today (account model, `lib.rs`) | Current realisation | RecordStore mapping |
|---|---|---|---|
| 1 | Entity | account at `entity_address(key) = key[:20]`; `code = 0xFE ‖ RLP(EntityRlp)` | record `"arkiv.entity" ‖ key` — **blob lane** (`blob` / `set_blob`); existence = `has_record` (the §6 idempotency primitive); removal = `delete_record` (blob-only record) |
| 2 | Pair bitmap (tier-1 reverse index) | account at `keccak("arkiv.pair" ‖ k ‖ 0x00 ‖ v)[:20]`; roaring64 bytes as `code` | record `"arkiv.pair" ‖ k ‖ 0x00 ‖ v` — **blob lane**; roaring encode/decode + set algebra stay engine-side (physical layout: §8) |
| 3 | Global entity counter | slot `keccak("entity_count")` on system account | **cell** on record `"arkiv.system"`, `CellKey = "entity_count"` |
| 4 | Minting nonces (`nonces(address)`) | slot `keccak("nonces" ‖ caller)` on system account | **cell** on `"arkiv.system"`, `CellKey = "nonces" ‖ caller` |
| 5 | id → entity map | slot `keccak("id_to_addr" ‖ id)` storing a 20-byte **address** | **cell** on `"arkiv.system"` storing the 20-byte **entity key** (§6) — same width, but a domain value instead of a host address |
| 6 | entity → id map | slot `keccak("addr_to_id" ‖ addr)` | **cell** on `"arkiv.system"`, keyed by entity key |
| 7 | Tier-2 Int index | in-storage **B+ tree** (header + node accounts, order 32, lazy delete) | **engine-composed over the data lanes (§9)**: numeric ranges become bitmap records (BSI slices or GE cumulatives); a value directory survives only if that candidate wins the design pass — then as engine-side cell records |
| 8 | Padded slot keys + `len+1` presence encoding | needed because slot keys are fixed 32 B | **gone** — §9 structures store values and bitmaps in blob records; no slot-key packing |
| 9 | Tier-2 Str index | 4-level cascade accounts + enumeration-list accounts (values ≤ 128 B) | **engine-composed prefix directory (§9)** — bounded bucket records replace the cascade + enumeration lists |
| 10 | `iter_storage_asc` | trait method; bails in the production write adapter | **no port equivalent — deliberately.** Ordered access is engine-composed over engine-known record keys (§9); adapters are point-op KV |
| 11 | Range query eval | Gt/Gte: iter from bound; Lt/Lte: full scan to bound; Str ranges: collect **all** values, filter in memory | bitmap algebra over blob records (§9): BSI = O(width) ops, GE = full-group cumulatives + boundaries — bounded by construction |
| 12 | EIP-161 workarounds (`ensure_account_persists`, tombstone nonce=1) | engine calls them explicitly | **adapter-internal** — no port equivalent |
| 13 | `0xFE` code prefix (stray-CALL guard) | engine prepends/verifies | **adapter-internal** (Ethereum representation detail) |
| 14 | keccak address derivation (`pair_address`, `int_index_address`, `str_level_address`, `btree_*_address`) | engine computes account addresses | **adapter-internal**: adapter maps `RecordKey` → location via `hash(key)[..20]`; engine composes namespaced keys only |
| 15 | Entity-key derivation keccak | engine calls `keccak256` directly | stays core, but via **`Host::hash()`** (spec-owned preimage, host-bound compression fn) |

Rows 1–2 (blob) and 3–6 (cells) are the data model; rows 7–11 become
engine-composed structures over those same lanes (§9); rows 12–14 stop
existing as engine concepts — they are what "the account model is the
port" was costing. Row 15 is the one deliberate keccak survivor (domain
spec, not representation).

Key decisions and motivation:

- **Named `RecordStore`, deliberately NOT `State`.** Whether data written
  through this port becomes part of the commitment is an **adapter policy
  (ρ), not a port property**: option (a) commits all lanes into the account
  trie; option (b) backs the index lane with a node-local store outside
  `state_root` — same engine code, the engine cannot tell, and that
  blindness is the feature. The port's contract is *determinism +
  transactionality* for every lane, committed or not (uncommitted index
  work is still metered, and cost feeds gas → consensus-critical).
  Genuinely non-consensus supporting components (caches, query
  accelerators, telemetry) must NOT sit behind this port on the write
  path — they live adapter-internal, permitted wherever they reproduce
  identical answers.
- **`RecordKey` = namespaced byte string composed by the engine**
  (`"arkiv.entity" ‖ key`, `"arkiv.pair" ‖ k ‖ 0x00 ‖ v`, …). The adapter
  maps keys to substrate locations — Ethereum: `hash(key)[..20]` account
  address; in-memory: the key itself. Keccak-for-addresses leaves the core.
  Namespace collision-freedom is an engine obligation; injectivity of the
  location mapping is an adapter obligation.
- **No index lane — indexes are engine-composed (DECIDED, §9).**
  Earlier drafts carried `index_insert` / `index_remove` /
  `index_iter_from` as a third lane so adapters could provide native
  ordered iteration. Dropped: BSI/GE remove the unbounded-iteration
  consumer for numerics, string prefix structures are buildable from
  bounded records, and a lane would force **every adapter** to
  implement consensus-critical iteration identically — n independent
  implementations of cursor semantics vs. one engine-side
  implementation over dumb KV. Ordering/boundedness obligations move
  into the engine's index structures (§9); the casualty (hybrid state
  option b) and its namespace-policy fallback are recorded there.
- **`hash()` via `Host`, supertrait of `RecordRead`** (baked into the
  sketch above). Hashing isn't storage — it gets its own trait — but a
  separate generic parameter threaded through `execute`, every op
  handler, and `$entityHash` maintenance would be noise; the supertrait
  bound lets one `S` carry both capabilities. With address derivation
  adapter-side, the core's only hash uses are entity-key derivation
  (`key = hash(preimage)[..20]`, §6) and `$entityHash` (§7) — this
  removes the last named-keccak from the core. Contract: pure,
  deterministic, collision-resistant (adapter obligation, same category
  as `time` monotonicity in §4), and **fixed per deployment**: the hash
  identity is a domain-profile constant — the SDK derives keys offline
  with the same function, so it is an *engine* implementation detail but
  NOT a *deployment* detail; changing it is a hard fork. The preimage
  uses the neutral spec tag `"arkiv.entity-key"` (§6) rather than any
  host address.
- **Two-trait split, `RecordRead` + `RecordStore: RecordRead`** (baked
  into the sketch above): `execute` binds the full `RecordStore`; read
  ports (§4) bind `RecordRead` — a compile-time guarantee that reads
  cannot mutate, and read-only adapters never implement writes.
  ("RecordWrite" as a name was rejected: a trait that includes reading
  via its supertrait shouldn't be called Write.)

Contract points (learned from the current adapters — write these into the
port spec):

1. **`delete_record` must NOT promise "wipes cells"** — cell enumeration is
   exactly what the MPT adapter cannot do. Contract: delete clears blob +
   existence; restrict engine usage to blob-only records (true today:
   entities and pair records have no cells) or document that unzeroed
   cells may persist invisibly.
2. **Reads take `&mut self`** — the reth adapter lazily loads and caches
   through a `&mut` database handle; `&self` reads force interior
   mutability into every adapter for aesthetics.
3. **Zero-word / empty-blob = unset is permanent contract** — the engine
   can never store a meaningful literal zero on any substrate (already
   practiced via sentinel encodings like `len+1` presence values). State
   it explicitly.
4. *(Retired.)* The former iteration contract (ascending byte-lex,
   bounded cursors) applied to the index lane, which no longer exists —
   ordering and boundedness are now engine-internal obligations of the
   §9 index structures, enforced once rather than per adapter.
5. Transactionality: reads see the transaction's own prior writes;
   commit/discard of the diff is owned by the host per the §4 inbound-port
   contract (`Outcome::Failure` ⇒ discard).
6. **Snapshot consistency (read adapters):** a `RecordRead` handed to a
   read port must present one immutable, consistent snapshot of committed
   state; *which* snapshot (which block) is host territory, decided at
   adapter construction. The evaluation time for query semantics is a
   separate, explicit port parameter (`QueryEnv.time`, §4) — the engine
   never infers it from the adapter.
7. **Record existence/creation semantics — UNSPECIFIED, must be pinned.**
   Today accounts materialise lazily; the sketch has `create_record` but
   doesn't say whether `set_cell`/`set_blob` on a non-created record
   auto-materialises it, nor what makes `has_record` true (explicit
   creation vs. any lane content). Pair records are never explicitly
   created today. Decide before implementing; interacts with §6's
   `has_record`-based idempotency check. *(Resolved for entity records in
   §7: existence = designated never-zero system cell — `$creator` slot.
   Still open for the general port contract.)*
8. **Reorg/rewind is a host/adapter obligation — the engine stays
   oblivious.** The engine only ever sees single-transaction execution
   against a snapshot; chain reorganisation never crosses the port.
   With indexes engine-composed over ordinary records (§9), all index
   state rewinds with committed state for free via the host's own
   rewind machinery. The obligation becomes non-trivial only if the §9
   hybrid exception (namespace-scoped node-local storage) is ever
   adopted — then those records must rewind consistently with the
   committed snapshot (scope-doc §6b's open spike, surfaced as a
   port-contract obligation, not an engine concern).

**Indexing at the port — RESOLVED: no index primitives.** The former
open item (index-lane design: primitive set, cursor shape, per-adapter
feasibility, lane metering) is retired — the lane was dropped once
BSI/GE removed the unbounded-iteration consumer. How indexes are built
from the seven port operations, the hybrid fallback, and the residual
open choices (numeric range encoding: value directory vs. BSI vs. GE;
string prefix structure) live in **§9**, which pairs with §8 as one
index design pass.

---

## 6. Entity key creation — salt as primary, nonce as fallback

### Current state

Entity keys are derived server-side from a per-caller counter; the client
cannot choose or influence the key.

- Derivation (`derive_entity_key`, `call.rs`):
  `entityKey = keccak256(chainId_be32 ‖ ARKIV_ADDRESS ‖ caller ‖ nonce_be4)`.
- The nonce is a u32 counter on the system account
  (`slot_nonces(caller)`), read-then-incremented by `bump_nonce` on every
  CREATE, exposed to the SDK via the `nonces(address)` view so clients can
  predict their next key.
- The `entityKey` field in the CREATE `Operation` is **ignored** — a client
  that populates it gets no error and a different key than it asked for.
- `create()` (`lib.rs`) writes the entity RLP **without checking** whether the
  target entity account is already occupied.

Consequences: creates are **not idempotent** (a retried CREATE after an
ambiguous timeout silently mints a second entity); key prediction requires a
`nonces()` state read and is invalidated by any concurrent CREATE from the
same account; applications cannot map external IDs to entity keys
deterministically.

### Change proposal

Repurpose the CREATE `Operation.entityKey` field as a **mode selector /
salt input** — zero means auto (today's counter path), non-zero is a
caller-scoped salt — with an **explicit mode byte** in the derivation
preimage separating the two key spaces. Derivation uses the injected
`Host::hash()` (§5), never a named hash function:

```
auto:  key = hash(domain ‖ TAG ‖ caller ‖ 0x00 ‖ nonce_be4)[..20]   # counter bumps
salt:  key = hash(domain ‖ TAG ‖ caller ‖ 0x01 ‖ salt32)[..20]      # counter NOT bumped

hash = Host::hash (§5) — bytes32 out; deployment-profile constant
       (Ethereum host binds keccak256)
TAG  = "arkiv.entity-key"   # neutral spec tag (replaces ARKIV_ADDRESS
                            # in the preimage)
```

**Entity keys are 20 bytes** — the first 20 bytes of the digest. Key and
MPT location coincide (`entity_address` derivation disappears); wire
formats keep `bytes32` fields (SDK-Stability) with the key left-aligned
and a zero tail enforced. Engine type: `EntityKey` newtype (§1 table).

**Trade-off — 20-byte keys.**

Pros:

- **Key = storage location.** No second identity layer, no address
  derivation step, and the unchecked-tail hazard disappears by
  construction.
- **Single-slot packing** with 8-byte timestamps (§7) — the hot metadata
  fits in three slots.
- **No dead bytes** in attributes, events, ID maps, or bitmap keyspaces.
- **Honest security claim.** The account was always found by 20 bytes;
  32-byte keys add zero collision resistance on this host.

Cons:

- **80-bit birthday bound becomes spec-level identity strength**,
  including targeted denial-of-create at ~2^80 work. Mitigation: the
  existence check turns a collision into a loud typed failure, and salt
  mode gives the victim an escape hatch.
- **Keys and addresses become same-width hex.** SDK/tooling **must**
  ship a distinct presentation encoding for keys (prefix/checksum) so
  the two are never confusable in UIs — otherwise funds will be sent to
  entity keys.
- **The width is permanent and federation-wide.** Remote refs (§1 tag 6)
  encode `key20`; widening later would invalidate every stored
  reference — the least reversible decision in this document.
- **A host-derived number enters the substrate-independent spec.**
  Accepted because the account model is the committed horizon (state
  options a/b) and no plausible alternative host gains from 32 B.

Rules:

- **Mode byte is mandatory.** Without it, a user could pre-occupy the key a
  future auto-create of their own would derive (self-DoS). With it, the two
  preimage spaces are disjoint by construction: salt-creates are
  idempotent, and auto-creates cannot collide with salt-creates (the
  only residual collision is the ~2^80 truncated-hash case handled
  below).
- **The counter does NOT increment on the salt path.** The counter is the
  allocator for the *auto namespace*, not a global creation count. Bumping it
  on salt creates would (a) spend a consensus storage write for nothing and
  (b) re-break auto-key prediction, since predicting the next auto key would
  again depend on pending creates of *both* modes. With no bump, `nonces()`
  keeps exactly its current meaning: the next auto-mode derivation input.
- **Existence check in `create()`**, with per-mode semantics:
  - salt mode → typed `EntityAlreadyExists(entityKey)` revert. This is the
    idempotency signal: after a timeout, a retry that hits it proves the
    earlier tx landed.
  - auto mode → statistically impossible (mode byte separates the
    preimage spaces; only a ~2^80 ground truncated-hash collision can hit
    it) — but fail as the **same typed revert**, not a fatal error: a
    fatal here would permanently wedge the account (nonce never advances
    past the collision), whereas a revert lets the victim proceed via
    salt mode.
- **`salt == 0` is a sentinel** (selects auto mode). Document it on the
  `Operation` ABI struct; SDK rejects an explicit user-supplied all-zero salt
  client-side so "I chose zero" and "I chose auto" can't be confused.

**Key lifecycle after delete — TO CONFIRM: salt keys are
creator-rebindable names; auto keys are single-use.** Deleting a
salt-mode entity frees its record, so the same `(caller, salt)`
derivation can re-create the key — *including after an ownership
transfer* (new owner deletes; the original **creator** can rebind the
key with new content). Auto-mode keys can never recur (the counter is
never rewound). Rather than dropping salt mode over this (which would
forfeit idempotent creates, app-defined keys, and cross-shard
predictability — the three reasons it exists), spec the two key classes
explicitly:

- **salt keys** = names in their creator's namespace — delete unbinds
  the name, only the creator can rebind it; `transfer` conveys the
  current binding (the entity), never the name;
- **auto keys** = single-use, never reused.

This is smaller than it looks: references are weak (§1) and content at
a key was never immutable anyway — the owner can rewrite everything via
`set` at any time. Rebinding only changes *who* controls content after
a delete. Apps needing bind-stability across transfers use auto keys or
pin `$entityHash` (§7). The index layer already supports rebinding
safely: entity IDs (the u64 bitmap handles) are monotonic and never
reused, so each rebinding is a **new incarnation** — the old ID is out
of every bitmap and resolves to nothing; stale index state can never
leak from a previous incarnation to the next, and the existence check
guarantees at most one live binding per key at any time. Rejected alternatives: tombstoning deleted salt
keys (unbounded per-key residue over churn — delete stops meaning
gone); a per-salt generation counter in the preimage (destroys offline
key prediction, the point of salt mode).

Motivation:

- **Idempotent creates.** Retry with the same salt → same key → loud typed
  failure instead of a silent duplicate entity.
- **Stronger determinism.** A salt key is a pure function of values the
  creator knows offline — no `nonces()` round-trip, no invalidation by
  concurrent creates, no batch-offset bookkeeping. Auto mode keeps a
  zero-burden default for users who don't care.
- **App-defined keys.** `salt = hash(external_id)` gives applications a
  deterministic external-ID → entity mapping.
- **Zero ABI churn, cheap migration.** The field already exists; existing
  SDK clients send zero today and land in auto mode with unchanged
  semantics. The derivation change (mode byte) is a break vs. today's
  formula — acceptable pre-launch, preferable to implicit length-based
  domain separation.
- **Familiarity preserved.** Caller-scoped salts are the CREATE2 idiom —
  as well understood in the Ethereum community as nonces.

SDK layering (informative, not consensus): tier 1 default `salt = 0` (auto,
no user burden); tier 2 SDK-internal random salt per create for safe retry
loops; tier 3 application-derived salts for external-ID mapping. Salt
uniqueness only matters within the caller's own namespace, so random 32-byte
salts need no coordination.

Follow-ups when implementing: new typed error in the `sol!` block; keep
`nonces(address)` view unchanged; tests for mode separation (same bytes as
salt vs. auto-derived never collide), salt-create idempotency, counter
untouched on salt path, mixed-mode batches; note salt-grinding (vanity
20-byte keys) as an accepted non-issue in the formal model.

---

## 7. Entity storage layout — payload in code, metadata in slots

### Current state

The whole entity — payload, creator, created_at_block, owner, expires_at,
content_type, key, attributes, last_modified_at_block — is one RLP blob
(`EntityRlp`) stored as account `code`, prefixed `0xFE`, committed via
`codeHash`. Consequences:

- **O(payload) mutations**: any single-field change (transfer = 20 bytes,
  extend = 8 bytes) decodes, re-encodes, **re-keccaks and rewrites the
  entire blob**, payload included.
- **DB bloat**: reth stores code by hash — every metadata mutation orphans
  the previous full blob in the bytecode table (shared-by-hash makes
  pruning hard). Ownership churn on large entities accretes dead copies.
- **O(payload) proofs**: proving any single fact ("who owns X?") via
  `eth_getProof` requires shipping the whole code blob — `codeHash`
  commits atomically, there is no field granularity.
- **Metering dishonesty (once metering lands, §10)**: a transfer's real cost is
  O(payload); either users pay megabyte prices for 20-byte changes or the
  cost schedule lies.
- Entity keys are 32 bytes on the wire but **effectively 20 bytes**: the
  MPT location is `key[:20]` and nothing verifies the stored key's tail
  against a requested key — 12 unenforced bytes carried in every
  attribute, event, map entry, and the RLP.

### Change proposal

**Split the entity across the two §5 lanes on one record** (Ethereum
binding: one account):

- **Payload → blob lane (account `code`)**, raw. `codeHash` becomes the
  payload's true content hash; identical payloads dedupe across entities.
  Drop the `0xFE` prefix under the no-EVM executor (nothing can execute
  code); note it as executor-dependent.
- **Metadata → cell lane (fixed packed storage slots)**:

  ```
  slot 0: flags/version(1) ‖ owner(20)  ‖ expires_at(8)   = 29/32
  slot 1: creator(20)      ‖ created_at(8)                = 28/32
  slot 2: key(20)          ‖ last_modified(8)             = 28/32
  slot 3: payload_hash(32)      — engine-maintained, see $entityHash
  slot 4+: content_type (len-prefixed, ≤ 128 B → ≤ 5 slots)
  slot 9+: attribute area (count header, packed entries)
  ```

- **Entity keys are 20 bytes (§6)** — the packing above depends on it;
  key and MPT location coincide, so the entity-account address derivation
  and the unchecked-tail hazard both disappear. Trade-off analysis lives
  in §6.
- **Existence = never-empty system slots.** `$creator` is written at
  create, never mutated, never zero → slot 1 non-zero is the existence
  witness. Resolves §5 contract point 7 for entity records (`has_record`
  = designated never-zero cell); an empty-payload entity (empty code) is
  still unambiguously existent.
- **`$entityHash` — single commitment over all entity content**, stored
  as a system attribute of type `bytes32` (§1 tag 7):
  - **Must be hierarchical**: `entityHash = hash(payload_hash ‖
    canonical(metadata ‖ attributes))`. A flat hash over content would
    re-hash the payload on every metadata change — reintroducing exactly
    the O(payload) cost this section eliminates. Hierarchical, the
    payload leg is precomputed; a transfer re-hashes a few hundred bytes.
  - **`payload_hash` is an engine-maintained metadata cell**, computed
    via `Host::hash()` (§5) at create/payload-update — when the engine
    already holds the bytes. It must NOT reuse the host's `codeHash`:
    that would silently assume the substrate's blob commitment equals the
    injected hash — a host leak. On Ethereum the cell duplicates
    `codeHash` (32 B redundancy per entity) — the price of the core not
    knowing what `codeHash` is.
  - **Not indexed — tamper evidence only (to confirm).** `$entityHash`
    exists so entity content is provable/tamper-evident (one storage
    proof), not for lookup — no pair bitmap. Indexing it would mint a
    near-singleton bitmap record per mutation (content hashes are
    unique per entity version), re-introducing exactly the orphan-churn
    pattern §7/§8 eliminate. Content-addressed lookup can be added later
    as an opt-in if demand appears.
  - The `EntityOperation` event's `entityHash` field (currently always
    zero) gains its defined meaning.
  - Canonical encoding of the metadata leg must be specified (field
    order, widths) — it is consensus-critical.

Motivation (beyond fixing the costs above):

- **Proof granularity: O(payload) → O(log state).** Each metadata field
  gets an individual storage proof via `eth_getProof` — a few hundred
  bytes regardless of payload size. Materially upgrades the watcher
  soundness story (scope-doc §6 membership proofs); `$entityHash` in a
  slot additionally allows proving *total* content with one storage
  proof.
- **Metering honesty**: transfer = 1 slot write + 2 bitmap updates +
  `$entityHash` maintenance; price matches work.
- Confirms the §5 two-lane model on real load (entity = blob + cells on
  one record).

Accepted costs / ripples:

- **Read amplification**: full-entity resolve = code read + ~4–8 slot
  reads instead of one code read; enters the §10 cost schedule.
- **§5 ripples**: mapping-table row 1 becomes blob + cells; contract
  point 1's `delete_record` restriction gains the carve-out "fixed slots
  + count-headered areas are enumerable by construction, so entity
  records remain wipeable"; contract point 7 resolved for entity records
  (above).
- **§1 ripples**: tags 3/6 widths (20 B / 28 B) — recorded there.
- `EntityRlp` shrinks to (or disappears in favor of) the slot-packing
  codec; per-field encode/decode replaces whole-blob RLP round-trips.
- ID maps and `$key` bitmaps shrink to 20-byte values.

---

## 8. Index storage layout — segmented bitmaps

*Status: OPEN — exploration with a preferred direction, not a decision.
Resolve together with §9 — index implementation over the record store —
as one "index handling" design pass (physical layout here, structure
composition there).*

### Current state

Every tier-1 pair bitmap is **one roaring64 blob stored whole** as account
code, content-committed via `codeHash = keccak(bitmap_bytes)`. Every
insert/remove therefore: read whole blob → deserialize → mutate one bit →
reserialize → **re-hash the whole blob** → rewrite the whole blob.

- **The `$all` bitmap makes this O(N) per create**: it contains every
  live entity, and every create rewrites and re-hashes it in full. Dense
  pair bitmaps (popular attribute values, common `$contentType`s, …)
  scale the same way.
- **Orphan-blob churn**: code is stored by hash, so every bitmap mutation
  orphans the previous serialized version — with the §7 fix removing
  entity-blob churn, whole-bitmap rewrites become the dominant DB-bloat
  source (every create orphans a full `$all` copy).
- **Membership proofs are O(bitmap)**: proving "entity e ∈ bitmap"
  requires shipping the entire blob (codeHash commits atomically).
- **Unmeterable**: write cost grows with index size, unboundedly — a cost
  schedule (§4) cannot honestly price "insert into bitmap" without a size
  bound.

### Change proposal (direction)

**Segment bitmaps along roaring's native container boundaries.** Roaring64
already partitions the u64 ID space into containers of 2^16 IDs (keyed by
the high 48 bits); each container serializes independently (array, bitmap,
or run encoding, ≤ 8 KiB). Store each non-empty container as its own
record:

```
segment record:  "arkiv.pair" ‖ k ‖ 0x00 ‖ v ‖ seg_be6   → blob = container bytes
directory:       which segments are non-empty — cells on the parent pair
                 record (bit-set of segments), or a dedicated
                 directory record (decide in the design pass)
```

Effects:

- **Write cost gets a hard bound**: an insert/remove touches exactly one
  container — ≤ 8 KiB re-hash/rewrite worst case, regardless of total
  index size. This is what makes bitmap writes honestly meterable (§10):
  a constant-bounded price per bitmap update.
- **The write pattern is asymmetric — state it explicitly.**
  **Creates are append-only**: entity IDs are monotonic (§6), so an
  insert always lands in the newest segment — the hot tail container.
  **Deletes (including expired-entity reaping) are random-access**: the
  retired ID lives wherever it was minted, so a delete edits a
  *historical* segment in the middle of the range. Older segments are
  therefore frozen **with respect to creates only** — they are
  rewritten exactly when a delete hits them, bounded to that one
  container (≤ 8 KiB), paid by the deleting transaction. For `$all`
  under create-heavy traffic this still eliminates the dominant churn
  (every create no longer rewrites everything), and delete-driven
  reopening is proportional to actual deletions, not to index size.
- **Membership proofs shrink to one segment**: prove the segment blob
  (≤ 8 KiB) instead of the whole bitmap — direct upgrade to the watcher
  soundness story, same direction as §7's proof-granularity win.
- Query evaluation unions container-wise — which is how roaring works
  internally anyway; the evaluator streams segments instead of
  deserializing one monolith. Point membership (`contains id`) reads one
  segment.

Trade-offs and open items for the design pass:

- **Small bitmaps are the common case** (most pair bitmaps hold few
  entities) and must not pay segment+directory overhead: inline the
  bitmap in the parent record's blob until a size threshold, then promote
  to segmented form. **Promotion must be deterministic** (consensus) —
  fixed threshold, defined hysteresis (or none: one-way promotion).
- **Directory representation**: cells-as-bitset on the parent record vs.
  a dedicated directory record; interacts with full-union read cost.
- **Canonical per-container serialization must be pinned — including
  roaring's container-encoding selection.** This is a consensus-specific
  landmine: the choice between **array / bitmap / run** encodings, and
  any `runOptimize`-style conversion heuristics, must be **mandated or
  forbidden by spec, never left to library defaults** — two correct
  roaring implementations legitimately disagree on when to run-encode,
  and here that disagreement is a state-root divergence. Today's
  determinism guarantee/test covers whole-treemap serialization only;
  the per-container equivalent (bytes *and* encoding choice) becomes
  consensus-critical.
- **Segment-key format** (`seg_be6` sketch = high 48 bits) and the record
  namespace; read amplification for full unions (many small records vs.
  one big blob) enters the §10 cost schedule.
- **The directory must prune fully-emptied segments** — entity IDs are
  monotonic and never reused, so without pruning the directory grows
  with `ever_created / 2^16` instead of with live data: a quiet
  monotonic structure. Related boundedness note: bitmaps only shrink on
  actual deletes — expired-but-unreaped entities remain in `$all` and
  every other bitmap (expiry filtering is query-side, §4) — so index
  size depends on GC actually running (§2 GC funding).
- Whether the same segmentation serves the `$linkedBy` aggregate (§1
  open item) and other future dense built-ins — likely yes, for free.
- Note the layering: segmentation is **engine-side layout over the §5
  blob lane** (the engine composes segment record keys) — the port is
  unchanged; adapters stay oblivious. Same pattern as §7: layout
  decisions live above the port, host mechanics below it.

Additional items from infra review (fold into the same design pass):

- **Cardinality analysis is a required input.** Per-exact-value posting
  lists + directory scan is a *low-cardinality* design; range queries
  over high-cardinality numerics degrade to O(distinct values in range)
  record reads regardless of result size. The doc currently assumes low
  cardinality everywhere without stating it. Sizing workload:
  `$expiration ≤ now` — the GC bots' scan (§2), near-worst-case
  cardinality on the permanent hygiene path.
- **Evaluate the numeric range-encoding candidates: BSI and GE**
  (summary and resolution: §9). BSI (Pilosa/FeatureBase): one bitmap per
  bit position → ranges O(64) bitmap ops independent of cardinality, no
  value directory, up to 64 bitmap touches per numeric write. GE
  (RABIT, SIGMOD 2025, `pacmmod25-wang.pdf`): point bitmaps stay as-is
  (tier-1 unchanged), plus one cumulative bitmap per spec-fixed group of
  contiguous values → ranges = full-group cumulatives + boundary
  points, ~2 bitmap touches per write, per-group value directory for
  boundaries; cost models in the paper support static group-size
  tuning. Both compose with the segmentation above (create inserts stay
  tail-appends). Only RABIT's encoding transfers — its background-flush
  /MVCC update architecture is consensus-inapplicable. Strings keep the
  ordered directory for prefix/glob in all candidates.
- **Fragmentation floor without compaction**: consensus forbids the
  background merges every Lucene/Druid-class system relies on;
  mostly-deleted mid-range segments never merge, so full-union read
  cost tracks *spread*, not liveness. Decide: opportunistic
  deterministic neighbor-merge on write vs. accept-and-meter. Softened
  in practice by cohort locality: with similar `btl`, entities created
  together expire together, so reaping sweeps old segments roughly in
  order and often empties them outright (directory pruning removes
  them).
- Positive observation to state: **every create-driven insert into ANY
  bitmap is a tail append** (new IDs are the maximum), not just `$all` —
  deletes are the only random access in the entire tier-1 write load.

---

## 9. Index implementation over the record store

*Decided: the port carries **no index primitives** — the former index
lane (`index_insert` / `index_remove` / `index_iter_from`) is dropped,
and all index structures are engine-composed over the two data lanes.
This section explains how, and owns the residual open choices. Pairs
with §8 as one index design pass.*

**Why no lane** (recap of the resolution): every candidate structure is
composable from cells and blobs; a lane would force every adapter to
implement consensus-critical ordered iteration identically (n cursor
implementations, each needing conformance coverage), whereas without it
adapters are deterministic point-op KV + hash and the ordered-index
logic exists exactly once, engine-side. Consequences: the port is seven
point operations; the iteration contract is retired (§5 point 4); index
metering is uniform record-op pricing (§10); index state rewinds with
committed state like any record (§5 point 8). The layering is the
classic DBMS shape: RecordStore = page-store abstraction, the engine =
the database building its trees and bitmaps on top.

**How each structure maps onto the seven port operations:**

| structure | records | port ops used |
|---|---|---|
| tier-1 pair bitmaps, segmented (§8) | segment blobs `"arkiv.pair" ‖ k ‖ 0x00 ‖ v ‖ seg`; segment directory as cells on the parent pair record | `blob`/`set_blob`, `cell`/`set_cell` |
| BSI slices (numeric ranges) | one bitmap family per bit position `"arkiv.bsi" ‖ attr ‖ j`, each segmented exactly like §8 | blob lane only — range eval is O(width) bitmap algebra over point reads |
| GE cumulatives (numeric ranges) | per-group bitmaps `"arkiv.ge" ‖ attr ‖ group`; the bounded per-group value directory as that group record's cells/blob | blob + cells — boundary resolution reads one group record |
| string prefix directory | bounded bucket records `"arkiv.sdir" ‖ attr ‖ bucket` holding sorted value lists; deterministic bucket scheme TBD (fixed-fanout trie of bounded buckets, or today's B+ tree re-hosted over cell records) | cells/blobs |

Common properties, and why no iteration primitive is missed: every scan
is over **engine-known record keys** (segments, slices, groups,
buckets) — never "enumerate whatever exists", which is the primitive
the port no longer offers; every scan is bounded by construction;
ordering and canonical-serialization obligations (byte-lex, §8's
roaring-encoding pinning) are engine-internal spec items, implemented
once and conformance-tested once (§11).

**Hybrid (state option b) — the one cost of dropping the lane.**
Adapters can no longer be handed "the index" to store node-locally,
because index records are indistinguishable from data records. If
uncommitted indexes are ever wanted, the mechanism is a
**namespace-scoped adapter policy**: the port contract blesses
designated record-key namespaces (e.g. `"arkiv.bsi.*"`, `"arkiv.ge.*"`)
as commitment-optional, and adapters may back those with a node-local
store — a documented layering exception, adopted only if in-consensus
index costs (already reduced by §7/§8/GE) prove insufficient. Default:
every record committed. If adopted, §5 contract point 8's rewind
obligation re-activates for those namespaces.

**Open (the §8+§9 design pass):**

- Numeric range encoding: value directory (status quo) vs. **BSI** vs.
  **GE** — comparison in §8's infra-review items; required input: a
  cardinality analysis with the GC's `$expiration ≤ now` scan (§2) as
  the sizing workload.
- String prefix structure: bucket scheme, deterministic split rules,
  bucket-size bounds (→ §3 limits).
- Requirements inherited from the current implementation: set semantics
  (idempotent insert, exact remove), value-length caps (→ §3), byte-lex
  ordering, prefix scans; the write path today never iterates —
  preserve that property.

---

## 10. Metering

*TODO — section reserved, content to be worked out.*

The budget/cost *plumbing* is defined at the inbound port (§4); this
section will own everything the plumbing carries: the cost schedule
(β_ω values per operation/lane), calibration methodology against real
adapter costs, `eth_estimateGas` interplay (cost must be reproducible
and monotone-ish under simulation), failure/abort accounting, and the
read-path metering stance. This is the scope-doc §5 headline question —
currently the least-designed consensus-critical subsystem.

---

## 11. Conformance corpus

*TODO — section reserved, content to be worked out.*

The δ_R ≡ δ differential-testing workstream the port architecture
exists to enable: same operation sequences driven through the in-memory
reference host and the Ethereum adapter, states compared. Corpus format,
coverage strategy (op semantics, limits §3, failure paths, cost §10),
CI integration. For a chain verified by re-execution, this is the
spec's enforcement mechanism, not tooling.

---

## 12. DB engine isolation — own repo, on triggers

### Current state

The core (`crates/arkiv-db-engine`) and the host side (`crates/arkiv-executor`,
`bin/arkiv-node`) live in one workspace in this repo (`arkiv-harness`), whose
name and README still describe a test harness — the engine's presence here is
an artifact of the executor spike, not a stated home.

- The dependency direction is already correct **at the crate level**: the
  engine's manifest has no reth/revm, only alloy primitives + roaring. Cargo
  enforces this per crate; the repo boundary would add no correctness.
- Nothing *guards* that property yet — one expedient `reth-ethereum = ...`
  line in the engine's Cargo.toml would compile fine.
- The engine currently exists **twice**: this crate was ported from
  `arkiv-op-reth` (`arkiv-entitydb` + the precompile), and the review-scope
  doc still points at the arkiv-op-reth copy as "the prototype". Two copies
  of consensus-critical logic in two repos will diverge silently.
- The port surface (inbound `execute` + read ports — §4; outbound
  `RecordStore` — §5) is under active redesign; every port change currently
  cuts across engine + executor + node in one atomic PR.

### Change proposal

**End state: the engine is maintained in its own repo** with zero
dependencies on any host repo, consumed by hosts as a pinned dependency.
Motivation:

- **Substrate independence made structural.** The engine is specified as a
  host-independent core (scope-doc §2/§4); a standalone repo makes that a
  fact of the codebase organisation, not just of the crate graph.
- **Multiple hosts.** A second host arrives soon: an **in-memory host** for
  running the engine in unit tests (and as the reference realisation for the
  conformance/commutativity corpus). Hosts multiply; the engine must not
  live inside any one of them.
- **Auditable artifact.** The security/architecture review is best scoped
  against a repo that *is* the engine (+ formal model + conformance tests)
  at a pinned commit, rather than directories inside a harness repo.
- **Ownership & cadence.** Engine (consensus-critical, spec-driven) and
  hosts (integration-driven) will want different maintainers, review bars,
  and release rhythms.

**But: do not split while the ports are fluid.** Splitting now would tax
exactly the highest-value current work — every port-API change would become
coordinated cross-repo PRs with version pinning churn and skew risk. Repos
should be split when the boundary is stable, not to make it stable.

**Extraction triggers** (split when these are met):

1. **Ports are final-final.** Inbound (`execute` + read functions, §4) and
   outbound (`RecordRead` / `RecordStore`, §5) have solidified to the
   point where we are *sure* they are final — i.e. the §4/§5 redesigns
   have landed, survived real use, and stopped moving.
2. **The in-memory host exists** as a genuine second consumer (unit-test /
   conformance host), proving the engine builds and runs with no host repo
   in sight.
3. External-consumer pressure: auditors need a scoped artifact, or SDK /
   conformance-corpus work wants to depend on the engine directly.
4. Ownership or release cadence actually diverges between engine and hosts.

Trigger 1 is necessary; 2 is expected soon and makes the split mechanical;
3–4 are accelerants.

**Do now (before any split):**

- **CI dependency guard** — a job that fails if the engine's dependency
  tree contains host crates (e.g. `cargo tree -p arkiv-db-engine` must not
  match `reth|revm`). Converts the architectural intent into a build
  failure; works in mono- and multi-repo alike.
- **Canonicalize the engine.** Decide the single source of truth vs. the
  `arkiv-op-reth` copy (`arkiv-entitydb`), freeze/deprecate the other, and
  update the review-scope doc's artifact pointers. This matters more than
  repo topology — divergent copies are the live hazard today.
- Keep the engine crate extraction-ready: self-contained, no path
  assumptions outside its directory (already true; `git filter-repo` /
  subtree split will then preserve history for free).

---

*Last updated: 2026-07-04.*
