# Arkiv harness

![rust-docker-build status](https://github.com/Arkiv-Network/arkiv/actions/workflows/rust.yml/badge.svg)
![python-lint status](https://github.com/Arkiv-Network/arkiv/actions/workflows/lint.yml/badge.svg)

Black-box test harness for the Arkiv database-chain. Stands up a devnet with kurtosis and drives it from the outside.

The Execution Layer is a dummy `arkiv-reth` (vanilla reth for now).

## Layout

```
arkiv/
├── bin/
│   ├── arkiv-reth/                     # dummy EL — vanilla reth wrapper
│   └── arkiv-cli/                      # black-box driver (skeleton)
│
├── crates/
│   ├── arkiv-bindings                  # Encoding helpers (entities, operations, attributes)
│   ├── arkiv-genesis                   # Genesis Chain Initialization params
│   ├── arkiv-harness                   # node client with shared config types
│   ├── arkiv-interfaces
│   ├── arkiv-query                     # Parser for Arkiv DB query language
│   ├── arkiv-reth-chainspec            # reth ChainSpec + the Arkiv minimum-base-fee rule, and the --chain parser
│   ├── arkiv-reth-executor             # Main EVM integration with modifications (e.g: smart contracts not supported)
│   ├── arkiv-reth-mpt-committed-store  # Implement storage
│   ├── arkiv-reth-rpc                  # available JSON RPC endpoints (read-only)
│   ├── arkiv-reth-statemanager
│   ├── arkiv-reth-uncommitted-store
│   └── arkiv-rpc-types                 # Shared request and response types for the JSON-RPC API
│
├── docker/                             # Dockerfiles for debian-slim
│   ├── arkiv-reth-dev.Dockerfile
│   └── arkiv-reth.Dockerfile
│
├── kurtosis/
│   ├── arkiv-chain.yaml                # ethereum-package args
│   └── integration/                    # bun + Arkiv SDK tests against the running enclave
│
├── scripts/
│   └── kurtosis/                       # up / down helpers
│       ├── check.py
│       ├── down.py
│       └── up.py
```



## Usage

> **Prerequisites:**
>
> - [Rust](https://rust-lang.org/tools/install/) installed
> - [Docker](https://www.docker.com/)
> - [Kurtosis](https://docs.kurtosis.com/install)  CLI

```sh
cargo build --workspace        # build
scripts/kurtosis/up.py         # build arkiv-reth image, run devnet
scripts/kurtosis/down.py       # tear down
```

`up.py` builds the `arkiv-reth` image and brings up a two-node Arkiv-only
network in one enclave via `ethereum-package`: one Lighthouse participant with
one validator (the sequencer) and one with zero validators (the follower).
There is no L1/L2 or settlement-chain coordination.

Scaffold only — the workspace builds and the devnet is wired; real harness logic comes next.

### Seeding the genesis state

A devnet can start with entities already in it. `arkiv-cli seed-genesis` builds
the block-0 state offline — synthetic entities pushed through the node's own
executor and store code, index included — and writes it in one of three shapes:

```sh
# A genesis file with the seeded accounts in `alloc` (dev chain id 1337, dev
# accounts funded): run it with --chain, dev mode still auto-seals on it.
arkiv-cli seed-genesis --count 2000 --payload-size 1024 --out seeded.json
arkiv-reth node --dev --chain seeded.json --http

# The ethereum-package shape: only the seeded accounts, as
# network_params.additional_preloaded_contracts. up.py does this for you
# (Kurtosis caps the package arguments at 4 MiB: ~1000 entities of 512 B):
ARKIV_SEED_COUNT=1000 scripts/kurtosis/up.py

# Beyond what a genesis file can hold in memory: a `reth init-state` dump and a
# genesis whose `stateHash` names the imported state's root. The seeder streams
# the dump and builds the root through an on-disk sort, so its memory is
# bounded by its batch and sort buffers, not by the seed; reth's importer
# streams too. (arkiv-reth's `init-state` is reth's, made to work at block 0:
# reth v2.5.0 alone refuses a genesis import under its default storage layout.)
arkiv-cli seed-genesis --count 1000000 --format jsonl --out state.jsonl
arkiv-reth init-state --chain state.jsonl.genesis.json --datadir /data state.jsonl
arkiv-reth node --dev --chain state.jsonl.genesis.json --datadir /data --http
```

Both long-running steps keep a progress file for a watcher, replaced whole
about once a second: `seed-genesis --progress-file <path>`, and `init-state`
always, at `<datadir>/init-state-progress.json` (or `$ARKIV_INIT_STATE_PROGRESS`).
Each is one JSON document with `pid`, `phase`, `percent` of the current phase,
`elapsed_s`, `updated_at` (unix seconds; stale for more than a few seconds
means the process is gone), the counters behind the percent, and at the end
`state_root` or `error`. The import's phases are `parsing` (bytes of the dump
read), `writing` (accounts written of the total) and `hashing` (the root
walk's position in the hashed key space), then `done` or `failed`.

Every run writes a manifest next to its output (`<out>.manifest.json`): the
chain id, counts, owners, the state root, the first entity keys, and each
owner's minting nonce after genesis. Entities are dealt round-robin to the
owners (`--owner 0x…`, repeatable; default the first dev account), keyed
exactly as the node keys creates, so an owner's next create after genesis
mints the next nonce's key. `--attribute name:type=expr` shapes the user
attributes (default `rank:u256=mod(100)` and `team:str=cycle(red,green,blue)`),
`--expires-at` sets one expiry block for all (default never); expired seeded
entities are purged by the protocol like any other.

The Kurtosis smoke test in CI runs on a seeded genesis of 1000 entities
(`kurtosis.yml`'s `seed_count` input), and `kurtosis/integration` checks both
nodes serve the seed from block 0.

## Development

CI runs three workflows: 

- `[rust.yml](./.github/workflows/rust.yml)`: 🦀 Rust checks and 🐳 Docker publishing for
`arkiv-reth` and `arkiv-reth-dev`.
- `[lint.yml](./.github/workflows/lint.yml)`: 🐍 Linting and formatting with [Black]((https://black.readthedocs.io)) over the Python scripts, using configs from `[pyproject.toml](./pyproject.toml)`.
- `[kurtosis.yml](./.github/workflows/kurtosis.yml)`: 🧪 the two-node sequencer/follower smoke test, followed by the SDK integration tests in `kurtosis/integration`.

Enable the local pre-commit hook with `pip install pre-commit && pre-commit install` — black then runs on staged Python before each commit.

## Releasing

Every merged commit on `main` is pushed to GHCR automatically as
`ghcr.io/arkiv-network/arkiv-reth` and `ghcr.io/arkiv-network/arkiv-reth-dev`,
tagged `sha-<short-commit>` and `latest`. Every tag is a manifest list covering
`linux/amd64` and `linux/arm64`. To cut a versioned release, tag a commit with
`vX.Y.Z` — the same images get pushed with that version tag.

**CLI** — tag the tip of main and push the tag:

```sh
git checkout main && git pull
git tag v0.2.0
git push origin v0.2.0
```

**GitHub UI** — [Releases](https://github.com/Arkiv-Network/arkiv/releases) → *Draft a new release* → *Choose a tag* → type the new `vX.Y.Z` and pick *Create new tag on publish* (target: `main`) → *Publish release*.

Either way, the build shows up under [Actions → rust](https://github.com/Arkiv-Network/arkiv/actions/workflows/rust.yml); images appear on GHCR when the `docker` job finishes (~1 h, cold build).