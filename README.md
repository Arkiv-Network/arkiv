# arkiv-harness

Black-box test harness for the Arkiv database-chain. Stands up a devnet with kurtosis and drives it from the outside.

The EL is a dummy `arkiv-reth` (vanilla reth for now), standing in for the real arkiv-op-reth execution client.

## Layout

```
bin/arkiv-reth/          dummy EL — vanilla reth wrapper
bin/arkiv-test-harness/  black-box driver (skeleton)
bin/arkiv-committer/     committer entrypoint (DA stub — owner Piotr)
crates/arkiv-harness/    shared config types
crates/arkiv-da/         frozen DA format — zstd-compressed RLP block (contract C2)
crates/arkiv-committer/  committer config + run loop (stub)
docker/                  arkiv-reth + arkiv-committer + arkiv-reth-dev Dockerfiles (debian-slim)
kurtosis/                arkiv-chain.yaml (ethereum-package args) + base-chain/ + committer/ packages
scripts/kurtosis/        up / down helpers
```

## Usage

Needs Rust, Docker, and the [kurtosis](https://docs.kurtosis.com/install) CLI.

```sh
cargo build --workspace        # build
scripts/kurtosis/up.py         # build arkiv-reth image, run devnet
scripts/kurtosis/down.py       # tear down
```

`up.py` brings up the stack in one enclave via additive kurtosis runs: the **Arkiv chain** (`ethereum-package` — arkiv-reth EL + lighthouse CL), a plain-reth **base chain** below it (DA / settlement substrate), and the **committer** wired to both. Separate runs because kurtosis can't import a remote package from a local one.

The committer (`arkiv-committer`) is **leg-work, not the committer itself** (owner Piotr): it links the frozen DA format (`arkiv-da` — `DA_VERSION || zstd(rlp(block))`), self-checks the round-trip, and heartbeats. The real poll-block → encode → post-to-inbox loop drops into the same slot; it waits on the inbox contract (Contracts) and the functional-MVP go-ahead.

Scaffold only — the workspace builds and the devnet is wired; real harness logic comes next.

## Development

CI runs two workflows: `rust.yml` (`cargo fmt --check`, `cargo build --workspace`, and `cargo nextest run --workspace`, with cargo caching, then a Docker build that pushes `ghcr.io/arkiv-network/arkiv-reth`, `ghcr.io/arkiv-network/arkiv-committer` and `ghcr.io/arkiv-network/arkiv-reth-dev` on `main` pushes and `v*` tags) and `lint.yml` (black over the Python scripts). Each image is built for `linux/amd64` and `linux/arm64` on a native runner per platform, and the two are joined into one manifest list per tag.

The Python helper scripts are formatted with [black](https://black.readthedocs.io); config lives in `pyproject.toml`.

Enable the local pre-commit hook with `pip install pre-commit && pre-commit install` — black then runs on staged Python before each commit.

## Releasing

Every merged commit on `main` is pushed to GHCR automatically as `ghcr.io/arkiv-network/arkiv-reth`, `ghcr.io/arkiv-network/arkiv-committer` and `ghcr.io/arkiv-network/arkiv-reth-dev` (an auto-sealing dev node carrying `arkiv-cli`), tagged `sha-<short-commit>` and `latest`. Every tag is a manifest list covering `linux/amd64` and `linux/arm64`, so a pull resolves to the architecture of the machine doing the pulling — an Apple Silicon Mac gets the arm64 image from the same name and tag a server uses for amd64. To cut a versioned release, tag a commit with `vX.Y.Z` — the same images get pushed with that version tag. The packages are private, so pulling needs `docker login ghcr.io` with a GitHub token carrying `read:packages`.

**CLI** — tag the tip of main and push the tag:

```sh
git checkout main && git pull
git tag v0.2.0
git push origin v0.2.0
```

**GitHub UI** — [Releases](https://github.com/Arkiv-Network/arkiv/releases) → *Draft a new release* → *Choose a tag* → type the new `vX.Y.Z` and pick *Create new tag on publish* (target: `main`) → *Publish release*.

Either way, the build shows up under [Actions → rust](https://github.com/Arkiv-Network/arkiv/actions/workflows/rust.yml); images appear on GHCR when the `docker` job finishes (~1 h, cold build).
