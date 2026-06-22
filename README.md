# arkiv-harness

Black-box test harness for the Arkiv database-chain. It stands up an execution client + consensus client devnet and drives it from the outside — no in-process node, only the public RPC / Engine / beacon surfaces a real deployment exposes.

For now the EL is a **dummy `arkiv-node`**: a vanilla reth node standing in for the real arkiv-op-reth execution client, so the harness has something to drive while the precompile/entity semantics land elsewhere.

## Layout

```
bin/arkiv-node/          dummy EL — vanilla reth wrapper (the custom EL image)
bin/arkiv-test-harness/  black-box driver (skeleton)
crates/arkiv-harness/    shared config / topology types
docker/                  arkiv-node Dockerfile (musl builder -> alpine runtime)
kurtosis/                ethereum-package devnet args
scripts/kurtosis/        up / down helpers
```

The Cargo workspace pins all crates to shared `[workspace.package]` metadata and borrows dependencies from the root `Cargo.toml`. reth is a git dependency pinned to tag `v2.2.0`.

## Prerequisites

- Rust (toolchain pinned in `rust-toolchain.toml`)
- Docker
- [kurtosis](https://docs.kurtosis.com/install) CLI

## Build & test

```sh
cargo build --workspace
cargo test --workspace
cargo run -p arkiv-test-harness     # prints the planned topology
./target/debug/arkiv-node --version
```

## Devnet

Orchestration is delegated to [ethpandaops/ethereum-package](https://github.com/ethpandaops/ethereum-package), which generates genesis, the JWT secret, validator keys and all wiring.

```sh
scripts/kurtosis/up.sh      # build arkiv-node:dev, then kurtosis run
scripts/kurtosis/down.sh    # tear the enclave down
```

`kurtosis/devnet.yaml` runs one `reth` EL (our `arkiv-node:dev` image) paired with a `lighthouse` CL. Add a second participant there to exercise follower / watcher keep-up.

Endpoints are assigned dynamically by kurtosis — discover them with `kurtosis enclave inspect arkiv-harness` or `kurtosis port print`. The defaults in `HarnessConfig` are placeholders until the harness reads them from the enclave.

## Status

Scaffold only: the workspace builds, the EL image is defined, and the devnet is wired. Real CL→EL block production assertions and enclave-driven harness logic are the next step.
