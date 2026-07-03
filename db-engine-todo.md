# DB Engine — TODO

*Growing task list for `crates/arkiv-db-engine` (and its executor seam). Each
section follows the same structure: **current state** — what the code does
today, with pointers — then **change proposal** with motivation. Companion
docs: `architecture-review-scope.md` (intended architecture),
`repo-code-structure.md` (code survey).*

---

## Contents

- [1. Entity key creation — salt as primary, nonce as fallback](#1-entity-key-creation--salt-as-primary-nonce-as-fallback)
- [2. Inbound port — `fn execute` (rename + reshape `dispatch`)](#2-inbound-port--fn-execute-rename--reshape-dispatch)
- [3. Outbound port — `RecordStore` (generic untyped record store)](#3-outbound-port--recordstore-generic-untyped-record-store)
- [4. DB engine isolation — own repo, on triggers](#4-db-engine-isolation--own-repo-on-triggers)

---

## 1. Entity key creation — salt as primary, nonce as fallback

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
preimage separating the two key spaces:

```
auto:  key = keccak256(chainId ‖ ARKIV_ADDRESS ‖ caller ‖ 0x00 ‖ nonce_be4)   # counter bumps
salt:  key = keccak256(chainId ‖ ARKIV_ADDRESS ‖ caller ‖ 0x01 ‖ salt32)      # counter NOT bumped
```

Rules:

- **Mode byte is mandatory.** Without it, a user could pre-occupy the key a
  future auto-create of their own would derive (self-DoS). With it, the two
  namespaces are disjoint by construction: auto-creates are infallible,
  salt-creates are idempotent.
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
  - auto mode → collision is impossible by construction; treat a hit as a
    fatal internal invariant violation (not a user-facing revert). Keeps
    defense-in-depth against the 160-bit address truncation and future code
    paths.
- **`salt == 0` is a sentinel** (selects auto mode). Document it on the
  `Operation` ABI struct; SDK rejects an explicit user-supplied all-zero salt
  client-side so "I chose zero" and "I chose auto" can't be confused.

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
`key[:20]`) as an accepted non-issue in the formal model.

---

## 2. Inbound port — `fn execute` (rename + reshape `dispatch`)

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
pub fn execute<S: RecordStore>(     // full outbound port (§3): read + write
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
    Operation { entity_key: B256, kind: OpKind, owner: Address, expires_at: u64 },
}

pub enum Failure {            // closed, typed set of deterministic failures
    BudgetExhausted,
    InvalidInput { .. },
    EmptyBatch,
    EntityNotFound { .. },
    NotOwner { .. },
    // ... one variant per current sol! error
}
impl Failure { pub fn to_revert_bytes(&self) -> Vec<u8>; }  // canonical ABI encoding
```

Key decisions and motivation:

- **`budget` enters via `ExecutionEnv`** — the enabler for §5 metering
  (running meter, abort on exhaustion → `Failure::BudgetExhausted`).
  Division of labor: host charges the intrinsic/byte floor and passes the
  remainder as the budget; engine owns state-dependent pricing.
  Invariant `cost <= budget`.
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
  encoded operation batches only.
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
// Bound on RecordRead (outbound port §3, read side): compile-time
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
}
```

Two notions of time, deliberately separated:

- **Snapshot selection** (*which committed state to read*) is host
  territory — decided at read-adapter construction, invisible to the
  engine (see §3 contract point on snapshot consistency).
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
  `RecordStore: RecordRead` — see §3. Read-side adapters (e.g. over a
  historical `StateProvider` for the watcher RPC) then implement only
  `RecordRead`; also resolves the current wart where
  `ExecutorStateAdapter::iter_storage_asc` bails on the write path.
- Read metering is out of scope here (reads are host/RPC territory for
  now) — but the unbounded-read-cost observation from the code survey
  (`Not`/glob queries materialising the full value universe) transfers to
  whoever serves these ports; revisit when the watcher RPC lands.

---

## 3. Outbound port — `RecordStore` (generic untyped record store)

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

// read side — what the read ports (§2 query / read_nonce) are bound on
trait RecordRead {
    fn has_record(&mut self, r: RecordKey) -> Result<bool>;
    fn cell(&mut self, r: RecordKey, c: CellKey) -> Result<Word>;   // zero word = unset
    fn blob(&mut self, r: RecordKey) -> Result<Bytes>;              // empty = unset
    fn index_iter_from(&mut self, i: IndexKey, from: Bytes, limit: u32)
        -> Result<Vec<Bytes>>;   // ascending byte-lex
}

// full store (read + write) — what `execute` (§2) is bound on
trait RecordStore: RecordRead {
    fn create_record(&mut self, r: RecordKey) -> Result<()>;
    fn delete_record(&mut self, r: RecordKey) -> Result<()>;

    // cell lane — fixed-width KV within a record
    fn set_cell(&mut self, r: RecordKey, c: CellKey, v: Word) -> Result<()>;  // zero clears

    // blob lane — one variable-length value per record
    fn set_blob(&mut self, r: RecordKey, b: Bytes) -> Result<()>;             // empty clears

    // index lane — ordered set of byte strings per index (see open item below)
    fn index_insert(&mut self, i: IndexKey, entry: Bytes) -> Result<()>;
    fn index_remove(&mut self, i: IndexKey, entry: Bytes) -> Result<()>;
}
// Word = fixed bytes (e.g. [u8; 32]); Bytes = arbitrary length.
// Note the lanes partition cleanly: iteration on the read side,
// insert/remove on the write side.
```

**Mapping table — current implementation → RecordStore.** Everything the
engine stores today fits the three lanes; the last rows are host mechanics
that stop crossing the port at all.

| # | Today (account model, `lib.rs`) | Current realisation | RecordStore mapping |
|---|---|---|---|
| 1 | Entity | account at `entity_address(key) = key[:20]`; `code = 0xFE ‖ RLP(EntityRlp)` | record `"arkiv.entity" ‖ key` — **blob lane** (`blob` / `set_blob`); existence = `has_record` (the §1 idempotency primitive); removal = `delete_record` (blob-only record) |
| 2 | Pair bitmap (tier-1 reverse index) | account at `keccak("arkiv.pair" ‖ k ‖ 0x00 ‖ v)[:20]`; roaring64 bytes as `code` | record `"arkiv.pair" ‖ k ‖ 0x00 ‖ v` — **blob lane**; roaring encode/decode + set algebra stay engine-side |
| 3 | Global entity counter | slot `keccak("entity_count")` on system account | **cell** on record `"arkiv.system"`, `CellKey = "entity_count"` |
| 4 | Minting nonces (`nonces(address)`) | slot `keccak("nonces" ‖ caller)` on system account | **cell** on `"arkiv.system"`, `CellKey = "nonces" ‖ caller` |
| 5 | id → entity map | slot `keccak("id_to_addr" ‖ id)` storing a 20-byte **address** | **cell** on `"arkiv.system"` storing the 32-byte **entity key** (fits `Word` exactly — one host leak removed) |
| 6 | entity → id map | slot `keccak("addr_to_id" ‖ addr)` | **cell** on `"arkiv.system"`, keyed by entity key |
| 7 | Tier-2 Int index | in-storage **B+ tree** (header + node accounts, order 32, lazy delete) | **index lane** `"arkiv.idx" ‖ attr_key`; entries = raw values; the B+ tree becomes the *Ethereum adapter's* `index_iter_from` implementation |
| 8 | Padded slot keys + `len+1` presence encoding | needed because slot keys are fixed 32 B | **gone** — index entries are variable-length `Bytes`, values stored as themselves |
| 9 | Tier-2 Str index | 4-level cascade accounts + enumeration-list accounts (values ≤ 128 B) | **gone** — same flat index lane as row 7; glob = iterate-from-prefix, stop at bound |
| 10 | `iter_storage_asc` | trait method; bails in the production write adapter | `index_iter_from` (read side, `RecordRead`) — bounded, cursored |
| 11 | Range query eval | Gt/Gte: iter from bound; Lt/Lte: full scan to bound; Str ranges: collect **all** values, filter in memory | bounded scans via `index_iter_from` for all cases — strictly better than today |
| 12 | EIP-161 workarounds (`ensure_account_persists`, tombstone nonce=1) | engine calls them explicitly | **adapter-internal** — no port equivalent |
| 13 | `0xFE` code prefix (stray-CALL guard) | engine prepends/verifies | **adapter-internal** (Ethereum representation detail) |
| 14 | keccak address derivation (`pair_address`, `int_index_address`, `str_level_address`, `btree_*_address`) | engine computes account addresses | **adapter-internal**: adapter maps `RecordKey`/`IndexKey` → location via `hash(key)[..20]`; engine composes namespaced keys only |
| 15 | Entity-key derivation keccak | engine calls `keccak256` directly | stays core, but via the **host-functions trait** `hash()` (spec-owned preimage, host-bound compression fn) |

Rows 1–2 (blob), 3–6 (cells) and 7–11 (index lane) are the data model;
rows 12–14 stop existing as engine concepts — they are what "the account
model is the port" was costing. Row 15 is the one deliberate keccak
survivor (domain spec, not representation).

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
- **Index lane instead of engine-side emulation.** The B+ tree / cascade /
  lists become the *Ethereum adapter's* private implementation of
  `index_iter_from`; the in-memory host uses a native ordered map; a custom
  store (state option c) uses real cursors. Scope is minimal: an ordered
  set of byte strings (the tier-2 value directory) — bitmaps stay
  engine-side. Bonus: the port boundary lands exactly on the state-option
  a/b line — "hybrid" becomes "this adapter backs the index lane with a
  node-local store", zero engine change.
- **`hash(&[u8]) -> Digest32` on a separate small host-functions trait**
  (not in `RecordStore` — hashing isn't storage). Used by the core only
  for entity-key derivation; the preimage layout stays spec-owned, the
  compression function is host-bound (Ethereum: keccak256). Collision
  resistance becomes an explicit adapter obligation (same category as
  `time` monotonicity in §2). While the derivation changes anyway (§1 mode
  byte): replace `ARKIV_ADDRESS` in the preimage with a neutral spec tag
  (e.g. `"arkiv.entity-key"`).
- **Two-trait split, `RecordRead` + `RecordStore: RecordRead`** (baked
  into the sketch above): `execute` binds the full `RecordStore`; read
  ports (§2) bind `RecordRead` — a compile-time guarantee that reads
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
4. **Iteration contract**: ascending byte-lex order (consensus-relevant —
   ordering feeds gas/receipts), bounded via `limit`/cursor — never
   "return everything"; iteration cost must be boundable *before* the work
   happens once metering (§2) exists.
5. Transactionality: reads see the transaction's own prior writes;
   commit/discard of the diff is owned by the host per the §2 inbound-port
   contract (`Outcome::Failure` ⇒ discard).
6. **Snapshot consistency (read adapters):** a `RecordRead` handed to a
   read port must present one immutable, consistent snapshot of committed
   state; *which* snapshot (which block) is host territory, decided at
   adapter construction. The evaluation time for query semantics is a
   separate, explicit port parameter (`QueryEnv.time`, §2) — the engine
   never infers it from the adapter.
7. **Record existence/creation semantics — UNSPECIFIED, must be pinned.**
   Today accounts materialise lazily; the sketch has `create_record` but
   doesn't say whether `set_cell`/`set_blob` on a non-created record
   auto-materialises it, nor what makes `has_record` true (explicit
   creation vs. any lane content). Pair records are never explicitly
   created today. Decide before implementing; interacts with §1's
   `has_record`-based idempotency check.

**OPEN — indexing needs a dedicated design pass before committing.** The
index lane above is a placeholder direction, not a decision: cover in more
detail (a) the exact primitive set (ordered set vs. ordered map, prefix/glob
support, cursor shape), (b) how tier-1 bitmaps and the tier-2 directory
interact across the port, (c) metering of index operations, (d) feasibility
per adapter (MPT emulation, node-local store for hybrid, native cursors).
Pick up here when revisiting.

Requirements already established by mapping the current implementation
(absorption check, 2026-07-03): set semantics (idempotent insert,
exact-entry remove); variable-length entries with a stated cap (128 B
today — the cap feeds metering and the MPT adapter's node layout);
ascending byte-lex iteration, bounded/cursored; prefix scans (glob) via
iterate-from-prefix + stop-at-bound. The write path never iterates today,
so iteration can stay read-side (as the trait split reflects). Payoff
confirmed by the mapping: the B+ tree becomes the Ethereum adapter's
`index_iter_from` implementation, and the padded-slot encoding, the
4-level string cascade, and the enumeration lists disappear entirely
(variable-length entries remove their reason to exist); Str-mode ranges
improve from collect-everything-and-filter to bounded scans.

---

## 4. DB engine isolation — own repo, on triggers

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
- The port surface (inbound `execute` + read ports — §2; outbound
  `RecordStore` — §3) is under active redesign; every port change currently
  cuts across engine + executor + node in one atomic PR.

### Change proposal

**End state: the engine is maintained in its own repo** with zero
dependencies on any host repo, consumed by hosts as a pinned dependency.
Motivation:

- **Substrate independence made structural.** The engine is specified as a
  host-independent core (scope doc §2/§3); a standalone repo makes that a
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

1. **Ports are final-final.** Inbound (`execute` + read functions, §2) and
   outbound (`RecordRead` / `RecordStore`, §3) have solidified to the
   point where we are *sure* they are final — i.e. the §2/§3 redesigns
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

*Last updated: 2026-07-02.*
