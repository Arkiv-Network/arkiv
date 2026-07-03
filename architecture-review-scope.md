# Arkiv — Architecture Review Scope

*Prepared for a security/architecture review engagement. Purpose: give enough of
the system's shape to prepare an initial call and a price estimate. Depth lives
in the linked artifacts (end of doc); this is the map, not the territory.*

---

## Contents

- [What Arkiv is](#what-arkiv-is)
- [Trust model (read first)](#trust-model-read-first)
- [1. High-level architecture (the side chain)](#1-high-level-architecture-the-side-chain)
- [2. Ports & adapters + host design](#2-ports--adapters--host-design)
- [3. Formal model](#3-formal-model)
- [4. Execution — two options for removing the EVM](#4-execution--two-options-for-removing-the-evm)
- [5. Fees, metering & DoS](#5-fees-metering--dos)
- [6. State, storage & commitment](#6-state-storage--commitment)
- [7. Bridge — a constrained gas faucet (no smart contracts)](#7-bridge--a-constrained-gas-faucet-no-smart-contracts)
- [What's in scope vs deferred](#whats-in-scope-vs-deferred)
- [Reference artifacts](#reference-artifacts)

---

## What Arkiv is

**Arkiv is a non-EVM, data-centric side chain.** It replaces Ethereum's
smart-contract execution with a custom, deterministic **database engine** —
entities with typed attributes, ownership, lifecycle, and queries — running on a
**reth host** with the EVM interpreter removed, committing to an Ethereum-style
state root, produced by a single sequencer, and bridged from Ethereum by a
**trusted relayer** rather than bridge contracts.

This is **not a standard EVM-rollup engagement**: it includes a custom execution
engine and a custom state/bridge on a borrowed host.

---

## Trust model (read first)

The review should be calibrated to the **V1** trust posture; later phases relax
it.

- **Single sequencer.** One node produces every block (no validator set).
  Liveness rests on it; decentralization is a later phase.
- **Trusted relayer bridge.** A hot wallet funds gas (GLM); the relayer can
  credit arbitrarily — same trust level as the sequencer, but bounded to the gas
  float (no stored user value).
- **Replay tier.** State is archiver-/watcher-verifiable by re-execution; there
  are **no fault or ZK proofs in V1**. Verifiability in V1 = keccak-MPT proofs
  (`eth_getProof`) **+ a trusted operator**, not a trustless proof system.

So the two headline trust assumptions are **the sequencer** and **the relayer**.
Everything else should be read against that backdrop.

---

## 1. High-level architecture (the side chain)

A **sovereign** chain — no rollup, committer, or deriver. A single sequencer
produces blocks; a consensus client (Lighthouse) drives the execution layer and
gossips blocks so non-voting **watchers** can replicate the chain.

```
   Ethereum (L1)
      │  deposit / withdraw
      ▼
   ┌────────── trusted relayer (hot wallet, off-chain accounting) ─────────────┐
   └───────────────────────────────────┬───────────────────────────────────────┘
                                        ▼  credit / burn
   ┌──────────────────────────── Arkiv side chain ─────────────────────────────┐
   │  sequencer (1 node)  ──Lighthouse CL──►  watchers (non-voting followers)  │
   │        │  Engine API                                                      │
   │        ▼                                                                  │
   │  reth host  ── EVM interpreter removed ──►  Arkiv DB engine (execution)   │
   │        │                                                                  │
   │        ▼  state                                                           │
   │  state root (keccak-MPT)  ·  MDBX storage  ·  re-exec / snapshot sync     │
   │        │                                                                  │
   │        ▼  arkiv_* query RPC  +  Ethereum JSON-RPC (unchanged SDK surface) │
   └───────────────────────────────────────────────────────────────────────────┘
```

The product constraint shapes everything: **SDK-Stability** — the SDK surface
end-users build against must not change or only change as little as possible. 
The chain therefore keeps speaking Ethereum's dialect (tx envelope, JSON-RPC, 
gas/receipts/logs) even though the EVM interpreter is gone.

---

## 2. Ports & adapters + host design

The engine is structured as a **ports-and-adapters** core so it stays
independent of the host and is testable on its own.

- **Core — the Arkiv DB engine.** All validation and business logic (entities,
  attributes, indexes, queries); no host/revm types. Today's prototype is the
  `arkiv-entitydb` crate.
- **Inbound port — `execute`.** The host hands the engine the caller, gas,
  block context, and the operation bytes; the engine validates → decodes →
  applies → returns a result. (Atomic, fail-fast per transaction.)
- **Outbound port — a state interface (`StateAdapter`).** The engine reads/writes
  state through this narrow trait; it must not expose reth internals.
  Two adapters exist: a production one over reth, and an in-memory one for tests.
- **Host (rented from reth, ~unchanged).** Networking/devp2p, txpool, JSON-RPC,
  MDBX storage, the engine/block driver, sync — all reused. The engine touches
  exactly one component (execution); the rest are executor-independent.

**Conformance criterion.** Correctness is defined as **commutativity**: running
the real implementation on the representation of a state must yield the
representation of the state the spec prescribes (`δ_R ≡ δ`). This is the basis
for a conformance corpus and is the formal handle for "the engine faithfully
realises the spec."

---

## 3. Formal model

The Arkiv DB engine has a **substrate-independent formal model** (a deterministic
state-transition spec) that the implementation is meant to realise. It gives the
auditor a **specification to review the implementation *against*** — uncommon, and
it focuses the work.

- **Data model (MOL E/R).** State `S` = a set of entities (key, typed attributes,
  owner/policy, lifecycle: created/expires/deleted). Queries are read-only over
  the active view.
- **State-transition function `δ`** — deterministic; total (explicit failure
  semantics); factors into input-translation (`Θ`) and atomic batch (`Δ`).
- **Resource accounting `β_ω`** — deterministic, per-operation cost; the basis
  for metering (see §5 on fees/DoS).
- **Commitment `C`** — §3.2 distinguishes **(a)** an engine-defined entity
  commitment `C(s,c)` (a pure function of state, portable) from **(b)** reusing a
  host's existing scheme, where the entity embedding `ρ(s)` is *folded into* the
  host's state root `C_R(R)` (substrate-specific). Arkiv on reth is case (b)
  today; a portable `C(s)` is a deferred item (see §4/§6).
- **Runtime realisation & export/import** — the contract binding `δ` to a concrete
  substrate (`R`, `ρ`, `δ_R`) and the laws that keep state portable.

---

## 4. Execution — two options for removing the EVM

Both keep reth as host, keep revm's framework/types ("neuter, not delete"), and
reuse the engine behind the `StateAdapter`. They differ in **where the engine
plugs in** — and that difference is the review's main execution axis.

| | **Option 1 — precompile** | **Option 2 — custom executor** |
|---|---|---|
| Engine runs as | a stateful **precompile** inside reth's stock executor | **the executor**, replacing the execution component |
| The interpreter | present, bytecode branch blocked | never invoked |
| Roughly | ≈ today's prototype, made "no-EVM" | needs working spike to validate |
| Per-tx semantics | inherits revm's call-frame / journal semantics | fully custom per-tx body (decode → route → apply → receipt) |

Option 1 is the lower-delta baseline; option 2 gives full control of the per-tx
body. Both must keep emitting Ethereum-shaped gas/receipts/logs (SDK-Stability).

**No smart contracts — by construction.** Arkiv accepts only transactions whose
`to` is a known Arkiv-operation address; **every other transaction
reverts**. No bytecode is ever deployed or executed, and **genesis contains no
contracts** either. So *can a crafted tx reach the EVM?* — is answered by the
admission rule, not by trusting a blocked branch:
in Option 1 the contract/interpreter path is unreachable because no tx can target
it; in Option 2 there is no interpreter to reach. "No contracts, ever (not even at
genesis)" is a stated, independently-checkable invariant, and it is what makes
"remove the EVM" coherent.

---

## 5. Fees, metering & DoS

A deliberate split:

- **The Arkiv DB engine prices accurately.** It alone knows the real cost of a transaction
  — which/how many storage ops were touched (e.g. several large indexes) —
  so **state-dependent fees are computed in the engine**, as a **running meter
  against the supplied budget, aborting on exhaustion** (so it never does more
  work than is paid for). This is consensus-critical and must be deterministic.
- **The host handles coarse DoS.** Admission (balance ≥ budget × price), a
  byte-level floor, block gas limits, mempool/p2p rate limits.

Note per execution option: **option 1** inherits Ethereum's calldata floor + fee
market (the byte-spam floor is *paid* for free); **option 2** owns more of that
floor (its custom executor must re-charge it, or it's bypassed). The headline DoS
question for the review: **can an attacker make the engine do work the funded
budget hasn't already paid to allow?** — i.e. is the meter-and-abort airtight,
and is the cost schedule calibrated to real (state-dependent) cost.

---

## 6. State, storage & commitment

### Sync model

A fresh node reaches the chain tip one of two ways today, and **both are
layout-agnostic** — they work for any state shape or commitment scheme:

- **Re-execution (staged sync)** — replay blocks through the Arkiv executor,
  recomputing state and validating each `state_root` against the (independently
  validated) header chain. Trustless, but slow.
- **Snapshot sync** — copy a pre-built database, then re-execute only the short
  tail. Fast, but **trust-the-source**: the snapshot is anchored to a header
  `state_root`, so a corrupt/tampered one fails loud when the node extends the
  chain (it cannot forge the canonical chain — you would diverge from the
  validated headers — but you trust the source for liveness/convenience). In V1
  the operator is trusted anyway, so an **operator-provided snapshot** is the
  natural fast path.

### Queryable-state shape & commitment — three options

Both the **entity data** and the **indexes** (queries need them — the store is
non-enumerable) must live somewhere. Three options, by where each goes:

| | **a) account model** (current) | **b) hybrid** | **c) custom** |
|---|---|---|---|
| Entity data | account-MPT (consensus) | account-MPT (consensus) | custom store / trie |
| Indexes | account-MPT (consensus) | node-local / derived | custom store |
| Re-execution + snapshot sync | ✓ | ✓ | ✓ (must re-own rewind/history) |
| Trustless fast P2P sync (future) | available | available (data; indexes rebuilt) | forfeited |
| Verifiable queries | sound **+ complete** (indexes committed) | **sound** only (indexes not committed) | custom (can be complete) |
| Write cost | high (index-in-consensus amplification) | low (no consensus index writes) | lowest (claimed) |
| An index bug is… | a consensus split (it's in `state_root`) | gas/consensus issue if it changes cost; else a local query error | a consensus issue (custom trie) |
| Commitment vs formal model | covers entities **+** indexes | commits exactly the entity state `S` | custom (could be portable `C(s)`) |
| Distance from today | — | close (lift indexes out of consensus) | far (redesign) |

- **a) Ethereum account model (current).** Entity data *and* indexes map to reth
  accounts/storage backed by the keccak-MPT; the index is two-tier (roaring-bitmap
  equality + an ART for ranges) committed as account state. Queries are provably
  **complete** (the index is committed) and the door to a future trustless P2P
  sync stays open. Cost: every entity op writes index state into consensus
  (write-amplification), and the index code is consensus-critical.
- **b) Hybrid.** Entity data stays fully in the account model — so the commitment
  covers exactly the formal state (active entities at block N) — but the **index
  bytes stay out of the committed `state_root`** (a node-local store, not the MPT).
  Crucially, **index manipulation still happens *inside* each transaction**, so the
  engine meters the real index work and **charges gas for it** (§5); deferring
  index updates to end-of-block (e.g. a post-block ExEx) would break accurate
  pricing and is therefore *not* the model. Because that cost feeds gas/receipts,
  the index **manipulation and its cost are consensus-critical** even though the
  index *data* is uncommitted — a cost-affecting index bug is still a consensus
  issue; only a wrong query *result* (data outside the commitment) is a local
  error. Queries are **sound** (every returned entity is checkable against
  committed state) but not **complete** (the index isn't committed). Upside vs (a):
  no index bytes in the commitment → smaller committed state, no MPT
  write-amplification, cheaper snapshots. **Open — needs a spike:** every node
  builds the index inline during execution (re-execution rebuilds it for free);
  confirm the node-local store is deterministic and meterable, and that a
  **snapshot** ships or rebuilds it (a freshly-synced node is not query-ready until
  the index is present) — and measure that rebuild cost.
- **c) Custom model.** A purpose-built store and commitment for both entity data
  and indexes (e.g. reth-2.0 lower-level tables + a custom entity trie), optimised
  for range / sort / bitemporal performance. Re-execution and snapshot sync still
  work, but because committed state **leaves the account keccak-MPT** the chain
  must **re-own reorg-rewind and historical reconstruction**, and the future
  trustless P2P sync option is **forfeited**. Highest performance ceiling, highest
  effort and risk, largest delta from reth.

**a** and **b** both keep entity data in the account model (they differ only in
index placement; a→b is a contained change); **c** is a redesign. In the §3
commitment terms, **a and b keep reth's inherited state root (case b)**, while
**c moves toward an engine-owned commitment (case a)**. **b** does not foreclose
**a**'s completeness — a committed index/accumulator can be added later if
provable-complete queries become a requirement. (In the execution brief's terms:
a/b ≈ "Path A", c ≈ "Path B".)

### Query serving & tamper-evidence

Queries (`arkiv_*`) are served by **watchers**, not only the sequencer, so query
load scales out independently of the block producer. A watcher is **untrusted for
results**; integrity is a **two-part client-side check**:

1. **Predicate match** — the returned result set is re-checked against the **query
   string**: every returned entity must actually satisfy the query's predicate, so
   a watcher cannot slip in non-matching entities.
2. **State membership** — each returned entity is verified to be **part of
   committed state at the queried block number**, via a membership proof against
   that block's `state_root` (`eth_getProof`-style), so a watcher cannot fabricate
   entities or return ones not live at that block.

Together these give **soundness**: results are real, live-at-block-N, and honor
the predicate. They do **not** give **completeness** — a watcher could still
*omit* a matching entity — unless the index itself is committed (option a), where
an index proof closes the gap. This is the operational meaning of the
"sound vs complete" row above.

---

## 7. Bridge — a constrained gas faucet (no smart contracts)

Arkiv runs **no smart contracts**; the bridge exists for one purpose: to **fund
gas for data operations**. V1 is **GLM-only** (the gas token). Arkiv-side balances
are gas-credit users *spend* on entity ops, not wealth they accumulate — the design
may go as far as **prohibiting user-to-user transfers**.

Mechanism: a **trusted relayer**, no bridge contracts on either side. A GLM deposit
on Ethereum lands in an operator-controlled wallet; the relayer **credits gas** on
Arkiv (mint = credit balance); unspent gas can be **withdrawn** (burn on Arkiv →
release GLM on Ethereum). The Arkiv-side logic is **custom native engine code**,
not Solidity.

**Honest risk profile.** Calling this "no stored value" would be misleading:

- **Gas credit is redeemable, so it is bearer value.** Because unspent gas can be
  withdrawn back to GLM, a credited balance is a **claim on the GLM float**. That
  float *is* the value at risk — effectively the bridge's TVL — even though no
  ERC-20s or user "savings" live on-chain. Prohibiting transfers limits value's
  *accumulation and circulation*, not its existence.
- **The withdraw path is a value-exit.** burn-on-Arkiv → release-on-Ethereum is a
  classic bridge exit, so exit-side risks apply — bounded to the **in-flight
  float** rather than an unbounded asset TVL, but present. Bounded ≠ benign.
- **Mint is only as sound as the deposit oracle.** The relayer credits gas from
  off-chain observation of L1 deposits, so the live attack surface is **deposit
  replay, L1-reorg double-credit, and mis-attributed/duplicated credits**. Mint
  must be gated on **finalized, de-duplicated** deposit data; over-mint produces
  **unbacked gas** — a claim on a float that isn't there.
- **Trusted relayer + hot-wallet custody.** The relayer can credit arbitrarily
  (same trust tier as the sequencer) and custodies the float in a hot wallet;
  compromise drains the float and/or mints unbacked gas.

Honest summary: the **blast radius is the GLM float plus the credit/withdraw
accounting** — smaller than a general asset bridge, but **real value with a real
exit path**, not "spam-only." Removing bridge *contracts* is also what makes
deleting the EVM coherent (no contract execution anywhere), so the bridge and
execution choices are linked.

---

## What's in scope vs deferred

**In scope (V1 architecture & threat model):** the side-chain topology and trust
model (§1); ports/adapters + host integration (§2); the formal model &
conformance (§3); both execution options (§4); fees/metering/DoS (§5);
state/storage/commitment incl. the sync model (§6); the gas-faucet bridge (§7).

**Deferred (V2/V3 — note, don't price):** an engine-owned/ZK-friendly commitment
(JMT/Poseidon) and trustless proofs; proof-based bridge exits; a BFT validator
set / decentralization; multi-token gas. These are *directional*; reviewing them
now would over-scope.

---

## Reference artifacts

- **Formal model** — [arkiv/architecture/formal-model.md](../arkiv/architecture/formal-model.md)
  (the spec: data model, `δ`, commitment, metering, runtime/export-import).
- **Engine ↔ host design** — [planning/reth-host-db-engine-core.md](reth-host-db-engine-core.md)
  (ports & adapters, the state interface, the reth realisation, formal grounding).
- **Execution decision brief** — [experiments/post-evm-execution-report.md](../experiments/post-evm-execution-report.md)
  (the two execution options, state/commitment paths, settlement, bridge;
  source-grounded against pinned commits).
- **Prototype** — the [arkiv-op-reth](https://github.com/Arkiv-Network/arkiv-op-reth)
  repo: crates [arkiv-entitydb](https://github.com/Arkiv-Network/arkiv-op-reth/tree/develop/crates/arkiv-entitydb)
  (engine + `StateAdapter`) and [arkiv-node](https://github.com/Arkiv-Network/arkiv-op-reth/tree/develop/crates/arkiv-node)
  (host integration, precompile, query RPC).
- **Litepaper** — product context and the V1/V2/V3 roadmap.