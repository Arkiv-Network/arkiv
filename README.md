# arkiv-harness

Black-box test harness for the Arkiv database-chain. Stands up a devnet with kurtosis and drives it from the outside.

The EL is a dummy `arkiv-node` (vanilla reth for now), standing in for the real arkiv-op-reth execution client.

## Layout

```
bin/arkiv-node/          dummy EL — vanilla reth wrapper
bin/arkiv-test-harness/  black-box driver (skeleton)
crates/arkiv-harness/    shared config types
docker/                  arkiv-node Dockerfile (debian-slim)
kurtosis/                devnet package — Arkiv chain + base chain
scripts/kurtosis/        up / down helpers
```

## Usage

Needs Rust, Docker, and the [kurtosis](https://docs.kurtosis.com/install) CLI.

```sh
cargo build --workspace        # build
scripts/kurtosis/up.sh         # build arkiv-node image, run devnet
scripts/kurtosis/down.sh       # tear down
```

The devnet (`kurtosis/devnet.yaml`) runs two chains in one enclave: the **Arkiv chain** (arkiv-node EL + lighthouse CL) and a plain-reth **base chain** below it (DA / settlement substrate for the committer, built later).

Scaffold only — the workspace builds and the devnet is wired; real harness logic comes next.
