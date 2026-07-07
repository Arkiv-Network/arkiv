# arkiv-harness

**Arkiv** is a decentralized, queryable database that lives *inside* an Ethereum
execution layer: on-chain **entities** — owned, expiring records with queryable
attributes — served over an `arkiv_*` JSON-RPC surface, with the same consensus and
state-root guarantees as an Ethereum chain.

This workspace holds two things:

1. **The Arkiv execution-layer database** — built as **reth-injectable crates**
   behind a host-agnostic **specification** ([`arkiv-interfaces`]). This is the
   entity store, the query index, the query language, and a **no-EVM executor** that
   replaces reth's transaction interpreter with a fixed-function entity state
   transition. No fork of reth — it's injected through reth's node-builder seams.
2. **The black-box harness** — an `arkiv-node` (a reth wrapper), a DA **committer**
   that posts blocks to a base chain, and kurtosis packages that stand a devnet up
   and drive it from the outside.

The database is being ported, module by module, from a proven prototype into this
clean, spec-conforming structure. **See [`docs/architecture.md`](docs/architecture.md)
for how it all fits together** — the spec/host boundary, the two-tier index, the
no-EVM executor, and the port roadmap.

## Architecture in one breath

- **`arkiv-interfaces`** is the *spec*: `#![no_std]`, zero external deps, just the
  traits a host implements (`EntityStore`, `AuxiliaryStore`, `TransactionExecutor`,
  `QueryProcessor`, …) and the plain data they exchange (`Entity`, the `Query` AST,
  `Op`). It names no reth, revm, or alloy. Whether Arkiv is hosted by reth or
  something else is a separate layer; **consensus is the host's concern, not the
  spec's**.
- **The reth host** implements that spec over reth's account/slot state model: an
  entity lives in an account's `code` (`0xFE || RLP`); the query index lives in
  keccak-derived accounts (equality bitmaps + range B-trees in storage slots).
- **The executor is no-EVM**: user programs never run. A call to the Arkiv address
  is decoded to entity operations and applied by a Rust state-transition function
  that returns an account diff reth commits — the interpreter is bypassed entirely.

## Crate map

| Crate | Role |
|---|---|
| **Spec** | |
| [`arkiv-interfaces`] | The host-agnostic Arkiv database spec — traits + plain data, no_std, zero-dep |
| `arkiv-constants` | Protocol-wide byte widths (`ADDRESS_LEN`, `WORD_LEN`) shared across crates |
| `arkiv-query` | The Arkiv query language — hand-rolled lexer + parser → the `Query` AST |
| **reth host** | |
| `arkiv-reth-executor` | The no-EVM executor: `ArkivExecutor` (entity STF) + `ExecutorState` (the reth write-path bridge) |
| `arkiv-reth-entitystore` | `EntityStore` over reth account `code` — the record codec + the `AccountCode` seam |
| `arkiv-reth-auxstore` | `AuxiliaryStore` — the query index: equality bitmaps (tier 1) + range B-trees (tier 2) |
| **Harness / DA** | |
| `arkiv-da` | The frozen DA wire format — `DA_VERSION || zstd(rlp(block))` |
| `arkiv-committer` | Committer config + run loop (posts blocks to the base chain; leg-work, owner Piotr) |
| `arkiv-genesis` | Genesis primitives (dev-funding alloc, chain constants) |
| `arkiv-harness` | Shared config / topology types for the black-box harness |
| **Binaries** | |
| `bin/arkiv-node` | The execution client — reth assembled with the Arkiv executor |
| `bin/arkiv-cli` | Client CLI — create / query entities against a node over JSON-RPC |
| `bin/arkiv-committer` | Committer entrypoint |
| `bin/arkiv-test-harness` | Black-box driver (skeleton) |

## Status

The **spec is in place** and the reth host is being filled in module by module.
Landed: the query parser, the entity record codec, the entity store's persistence
seam and its reth write-path bridge, and the query index's equality + integer-range
primitives — each unit-tested behind a mock seam, plus an end-to-end write-path
integration test over real reth types. Still ahead: the index's string range +
`evaluate`/`apply_delta`, ABI op decoding, wiring the executor to the reth stores,
and the RPC surface. The full roadmap and what's done lives in
[`docs/architecture.md`](docs/architecture.md#the-port-roadmap).

## Build & test

Needs Rust (see `rust-version` in `Cargo.toml`).

```sh
cargo build --workspace
cargo nextest run --workspace   # or: cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

The `arkiv-interfaces` / `arkiv-query` / `arkiv-constants` crates stay `no_std` and
dependency-light on purpose; the reth-host crates pull the reth stack (`reth-ethereum`,
pinned to a tag — a git dependency, so Docker builds resolve it in isolation).

## Devnet (kurtosis)

Also needs Docker and the [kurtosis](https://docs.kurtosis.com/install) CLI.

```sh
scripts/kurtosis/up.py     # build the arkiv-node image, run the devnet
scripts/kurtosis/down.py   # tear it down
```

`up.py` brings up one enclave via additive kurtosis runs: the **Arkiv chain**
(`ethereum-package` — arkiv-node EL + lighthouse CL), a plain-reth **base chain**
below it (the DA / settlement substrate), and the **committer** wired to both.
(Separate runs because kurtosis can't import a remote package from a local one.)

The committer links the frozen DA format (`arkiv-da`), self-checks the round-trip,
and heartbeats; the real poll-block → encode → post-to-inbox loop drops into the
same slot (owner Piotr).

## Development

CI runs `rust.yml` (`cargo fmt --all --check`, `cargo build --workspace --locked`,
and `cargo nextest run --workspace --locked`) and `lint.yml` (black over the Python
scripts).

Python helper scripts are formatted with [black](https://black.readthedocs.io)
(config in `pyproject.toml`). Enable the local hook with
`pip install pre-commit && pre-commit install`.

## Docs

- [`docs/architecture.md`](docs/architecture.md) — how the database is built: the
  spec/host boundary, entities, the two-tier index, the no-EVM executor, and the
  module-by-module port roadmap.

[`arkiv-interfaces`]: crates/arkiv-interfaces
