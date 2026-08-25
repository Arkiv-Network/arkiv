# Arkiv harness

[![rust-docker-build status](https://github.com/Arkiv-Network/arkiv/actions/workflows/rust.yml/badge.svg)][rust-docker-build-ci]
[![python-lint status](https://github.com/Arkiv-Network/arkiv/actions/workflows/lint.yml/badge.svg)][gh-python-lint]

[rust-docker-build-ci]: https://github.com/Arkiv-Network/arkiv/actions/workflows/rust.yml
[gh-python-lint]: https://github.com/Arkiv-Network/arkiv/actions/workflows/lint.yml

Black-box test harness for the Arkiv database-chain. Stands up a devnet with kurtosis and drives it from the outside.

The Execution Layer is a dummy `arkiv-reth` (vanilla reth for now).

## Layout

```
arkiv/
├── bin/
│   ├── arkiv-reth/                     # dummy EL — vanilla reth wrapper
│   └── arkiv-cli/                      # black-box driver (skeleton)
├── crates/
│   ├── arkiv-bindings
│   ├── arkiv-genesis
│   ├── arkiv-harness                   # shared config types
│   ├── arkiv-interfaces
│   ├── arkiv-query
│   ├── arkiv-reth-executor
│   ├── arkiv-reth-mpt-committed-store
│   ├── arkiv-reth-rpc
│   ├── arkiv-reth-statemanager
│   ├── arkiv-reth-uncommitted-store
│   └── arkiv-rpc-types
├── docker/                             # Dockerfiles for debian-slim
│   ├── arkiv-reth-dev.Dockerfile
│   └── arkiv-reth.Dockerfile
├── kurtosis/  
│   └── arkiv-chain.yaml                # ethereum-package args
├── scripts/
│   └── kurtosis/                       # up / down helpers
│       ├── check.py
│       ├── down.py
│       └── up.py
```

## Usage

> **Prerequisites:**
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

## Development

CI runs three workflows: 
- `rust.yml`: 🦀 Rust checks and 🐳 Docker publishing for
`arkiv-reth` and `arkiv-reth-dev`.
- `lint.yml`: 🐍 Linting and formatting with [Black]((https://black.readthedocs.io)) over the Python scripts, using configs from [`pyproject.toml`](./pyproject.toml).
- `kurtosis.yml`: 🧪 the two-node sequencer/follower smoke test.

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
