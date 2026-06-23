# arkiv-harness

Black-box test harness for the Arkiv database-chain. Stands up a devnet with kurtosis and drives it from the outside.

The EL is a dummy `arkiv-node` (vanilla reth for now), standing in for the real arkiv-op-reth execution client.

## Layout

```
bin/arkiv-node/          dummy EL — vanilla reth wrapper
bin/arkiv-test-harness/  black-box driver (skeleton)
crates/arkiv-harness/    shared config types
docker/                  arkiv-node Dockerfile (debian-slim)
kurtosis/                arkiv-chain.yaml (ethereum-package args) + base-chain/ package
scripts/kurtosis/        up / down helpers
```

## Usage

Needs Rust, Docker, and the [kurtosis](https://docs.kurtosis.com/install) CLI.

```sh
cargo build --workspace        # build
scripts/kurtosis/up.py         # build arkiv-node image, run devnet
scripts/kurtosis/down.py       # tear down
```

`up.py` brings up two chains in one enclave via two kurtosis runs: the **Arkiv chain** (`ethereum-package` — arkiv-node EL + lighthouse CL) and a plain-reth **base chain** below it (DA / settlement substrate for the committer, built later). Two runs because kurtosis can't import a remote package from a local one.

Scaffold only — the workspace builds and the devnet is wired; real harness logic comes next.

## Development

CI runs two workflows: `rust.yml` (`cargo fmt --check` + `cargo build --workspace`, with cargo caching) and `lint.yml` (black over the Python scripts).

The Python helper scripts are formatted with [black](https://black.readthedocs.io); config lives in `pyproject.toml`.

Enable the local pre-commit hook with `pip install pre-commit && pre-commit install` — black then runs on staged Python before each commit.
