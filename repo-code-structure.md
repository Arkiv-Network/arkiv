# Repo Code Structure — findings log

*Accumulating doc for code-level findings in `arkiv-harness`, branch
`kf/merge-db-engine-into-custom-exec`. Companion to
`architecture-review-scope.md` — that doc is the intended architecture; this
one records what the code actually does today.*

---

## Workspace map

```
bin/arkiv-node/           reth host binary — assembly point, overrides ONE component (executor)
bin/arkiv-test-harness/   black-box driver — print-only skeleton, no orchestration yet
bin/arkiv-committer/      committer entrypoint (DA stub, out of review scope)

crates/arkiv-db-engine/   THE ARKIV DB ENGINE (core) — no reth/revm deps
crates/arkiv-executor/    THE EXECUTION ADAPTER — reth executor seam + production StateAdapter
crates/arkiv-harness/     shared harness config types
crates/arkiv-da/          frozen DA format (zstd(rlp(block))), out of review scope
crates/arkiv-committer/   committer run loop (stub), out of review scope
```

reth is a **git dependency pinned to tag v2.2.0** (unforked, "rented host").
The EVM-removal strategy is **Option 2 (custom executor)** from the scope doc
§4: the engine *is* the executor; the interpreter is never invoked for user
transactions.

## Where the two halves live

| Scope-doc concept | Code location |
|---|---|
| **Arkiv DB engine (core)** | `crates/arkiv-db-engine` |
| **reth host** | git dep `reth v2.2.0`, assembled in `bin/arkiv-node/src/main.rs` |
| Inbound port `execute` | `arkiv_db_engine::dispatch(state, ctx, calldata)` (`src/call.rs`) |
| Outbound port `StateAdapter` | trait in `crates/arkiv-db-engine/src/lib.rs` (~line 582) |
| Production adapter | `crates/arkiv-executor/src/state_adapter.rs` (`ExecutorStateAdapter` over reth `Database`) |
| Test adapter | `arkiv_db_engine::test_utils::InMemoryStateAdapter` (feature `test-utils`) |
| Injection point | `EthereumNode::components().executor(ArkivExecutorBuilder)` in `main.rs` |

## Crate details

### crates/arkiv-db-engine (core, 2218 + 637 + ~1770 query lines)

Dependencies: `alloy-primitives`, `alloy-rlp`, `alloy-sol-types`, `roaring`,
`eyre`, `tracing`. **No reth, no revm.** State model (all in the host's
account trie — scope-doc state option (a)):

- **Entity account** at `entity_address(key) = key[:20]`; entity RLP stored as
  account `code`, prefixed `0xFE` (EVM INVALID opcode).
- **Pair account** per `(annot_key, annot_val)`: roaring64 bitmap of entity IDs
  as `code` → content-addressed via `codeHash`.
- **System account** (`0x44…46`): global entity counter, per-caller nonces,
  ID↔address maps as storage slots. Lazily materialised
  (`ensure_account_persists` bumps nonce to 1 against EIP-161 pruning).
- **Tier-2 indexes** for range/glob queries: in-storage **B+ tree**
  (`AnnotMode::Int`, values ≤ 32 B, order-32 nodes, lazy delete) and a
  **4-level string cascade** with enumeration lists (`AnnotMode::Str`,
  values ≤ 128 B).
- Built-in annotations: `$all`, `$creator`, `$owner`, `$key`,
  `$createdAtBlock`, `$expiration`, `$contentType` (fixed-width BE encoding
  where lex order must equal numeric order).

Modules:
- `lib.rs` — state model, `StateAdapter` trait, op handlers (`create`,
  `update`, `extend`, `transfer`, `delete`, `expire`), B+ tree, in-memory
  test adapter, ~30 unit tests.
- `call.rs` — ABI surface (mirror of `EntityRegistry.sol` via `sol!`):
  `execute(Operation[])` + `nonces(address)`, selector dispatch, validation
  (ident charset, UTF-8, null bytes), typed revert errors, `EntityOperation`
  event, entity-key derivation `keccak(chainId ‖ ARKIV_ADDRESS ‖ owner ‖ nonce)`.
  Batch semantics: **fail-fast, atomic** — first op revert reverts the tx.
- `query/` — lexer → parser → tree-walking interpreter producing a `Bitmap`;
  `execute()` adds pagination (descending entity-ID order, cursor). Eq/In hit
  tier-1 pair bitmaps directly; ranges/globs enumerate tier-2 then union
  tier-1 bitmaps. 624-line eval test suite (in-memory adapter only).

### crates/arkiv-executor (adapter/host side, 313 + 235 lines)

- `ArkivEvm` implements alloy-evm's `Evm` trait, wrapping `EthEvm` but
  **replacing `transact_raw` entirely** with `arkiv_transact`:
  - `TxKind::Create` → hard error ("contract creation is disabled").
  - Call to `ARKIV_ADDRESS` → `arkiv_db_engine::dispatch` through a fresh
    `ExecutorStateAdapter`; success merges the entity-state diff; revert keeps
    only sender accounting (nonce bump + gas debit).
  - Any other call → **plain value transfer** (succeeds!).
  - System calls (EIP-4788/2935) delegated to inner revm.
- Gas: `intrinsic_gas()` re-implements 21000 + calldata tokens and the
  **EIP-7623 floor** — the §5 "option 2 must re-charge the byte floor" point
  **is addressed**. Fees are debited from sender but **not credited to any
  coinbase** (burned). `eth_estimateGas` handled by returning `Halt` when
  `gas_limit < intrinsic`.
- `ExecutorStateAdapter`: lazy read-through cache over reth `Database`,
  accumulates writes, `into_evm_state()` harvests the diff (storage slots
  carry original+current for revm's change tracking). Builds `Bytecode`
  without the EVM analyzer (skips O(N) `analyze_legacy` for bitmap data).
  **`iter_storage_asc` bails** — range queries are impossible through the
  production adapter (write path by design, but no read-path adapter exists).
- `ArkivExecutorBuilder` → `EthEvmConfig<ChainSpec, ArkivEvmFactory>`; block
  building/validation, receipts, header logic all remain stock reth.

### bin/arkiv-node (44 lines)

Stock reth CLI + `EthereumNode` types + default add-ons; overrides exactly the
executor component. **No `arkiv_*` RPC namespace registered** — the query
engine has no network surface yet.

### Remaining crates

`arkiv-harness` (config types), `arkiv-da` (frozen `DA_VERSION ‖ zstd(rlp)`
format), `arkiv-committer` (heartbeat stub) — out of the execution review
scope. `bin/arkiv-test-harness` prints planned topology only; the node smoke
test is a placeholder.

---

## Ports-and-adapters assessment

### Close already ✅

1. **Dependency direction is exactly right.** The core has zero host deps;
   the executor depends on the engine, never the reverse. The §2 claim
   "no host/revm types in the core" holds at the crate-graph level.
2. **Both adapters exist as prescribed** — production (`ExecutorStateAdapter`
   over reth) and in-memory (tests) — matching §2's two-adapter design.
3. **Inbound port shape matches** §2's `execute`: host passes caller + block
   context + raw operation bytes; all ABI knowledge lives engine-side;
   executor never sees ABI types. Atomic fail-fast per tx: ✓.
4. **Host touched in exactly one component.** One builder call in `main.rs`;
   networking, txpool, RPC, MDBX, engine-API all stock. The "rented host"
   discipline is real (reth is an unpatched git tag).
5. **Option 2 is a working spike, not a plan.** Custom per-tx body
   (decode → route → apply → receipt), Create rejected, interpreter
   unreachable for user txs, system calls delegated. The scope doc's "needs
   working spike to validate" is validated by this branch.
6. **The §5 option-2 byte-floor concern is addressed** (EIP-7623 floor
   re-charged in the custom executor).
7. **Determinism is a visible concern**: bitmap-serialization determinism
   test, `expire ≡ delete` state-path equality test, fixed-width BE encodings
   for range-ordered values.

### Major gaps ❌

1. **No metering at all — the §5 headline is unimplemented.** `CallContext`
   is `{caller, chain_id, block_number}`: **no gas budget crosses the port.**
   Every `execute()` costs intrinsic calldata gas only, regardless of how many
   index accounts, B+ tree splits, or bitmap rewrites it touches. The
   "running meter against the supplied budget, aborting on exhaustion" does
   not exist; state-dependent work is free beyond the byte floor. This is
   the scope doc's headline DoS question, currently answered "no".
2. **The port is at account altitude, not entity altitude.** `StateAdapter`
   is `code/set_code/storage/set_storage` over `Address`/`B256` — the
   Ethereum account model *is* the port. The core hard-codes host-state
   semantics: EIP-161 pruning workarounds (`ensure_account_persists`,
   tombstone nonce=1), the `0xFE` code prefix, keccak-derived addresses.
   Consistent with state option (a) and §3 case (b) (ρ folded into the host
   root), but the code implements δ_R directly — there is no
   substrate-independent δ. Moving to state option (b)/(c) means rewriting
   the core, not swapping an adapter. If §3's substrate independence is a
   real goal, the port needs to rise to entity/index operations.
3. **The read path is unwired.** Query lexer/parser/interpreter exist and are
   well tested, but only against the in-memory adapter. No `arkiv_*` RPC in
   the node, and no production read adapter (`iter_storage_asc` bails in
   `ExecutorStateAdapter`; a `StateProvider`-backed read adapter is missing).
   The watcher/query-serving story (§6) has no production code path yet.
4. **No conformance/commutativity harness.** §2 defines correctness as
   δ_R ≡ δ, but there are no differential tests running the same op sequence
   through `InMemoryStateAdapter` and `ExecutorStateAdapter` and comparing
   state. The building blocks are all present — this is low-hanging fruit.
5. **Admission-rule divergence from the scope doc.** §4 states "every
   transaction whose `to` is not a known Arkiv-operation address reverts".
   The code **allows plain value transfers to arbitrary addresses** (needed
   for gas funding, presumably — but then the doc's invariant needs
   restating). Related smells: `value` sent along with an `ARKIV_ADDRESS`
   call is debited from the sender and credited nowhere (silently destroyed);
   gas fees have no coinbase recipient.
6. **Stale porting-era comments contradict the code.** `arkiv-executor`'s
   Cargo.toml and `arkiv-node`'s main.rs both still say "stock Ethereum
   executor with a precompile registered at ARKIV_ADDRESS" — the code has
   moved past that (fully custom `transact_raw`). `lib.rs` header still
   references `arkiv_node::precompile` / op-reth / `EvmInternals`. Misleading
   for any reviewer reading top-down.

### Smaller observations

- Neutering happens at execution only; the stock txpool will still accept
  Create transactions (they fail at build/validation via `EVMError::Custom`).
  Deterministic, but a pool-level admission rule would match §5's
  "host handles coarse DoS" split better.
- `Neq`/`NotIn`/`Not`/`NotGlob` and all Str-mode ranges/globs materialise the
  **full value universe** (`$all` bitmap or `str_collect_all` DFS) — fine
  unmetered, but exactly the kind of state-dependent cost §5 says must be
  priced (read path today, but `dispatch` could grow reads later).
- B+ tree uses lazy deletes; nodes never shrink/merge — index accounts grow
  monotonically per attribute key. Unpriced state growth.
- `dispatch` returns `Err` (fatal) on unknown attribute `valueType` instead
  of a typed revert — a malformed-input class that kills the tx via
  `EVMError::Custom` rather than reverting; worth unifying.
- Entity-key derivation commits to `chain_id` and caller nonce (replay-safe
  across chains); `bump_nonce` overflows checked at u32.

---

*Last updated: 2026-07-02, initial survey of the executor + engine merge
branch.*
