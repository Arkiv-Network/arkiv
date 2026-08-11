# syntax=docker/dockerfile:1.7
#
# Dev image: `arkiv-reth` + `arkiv-cli`, defaulting to an auto-sealing dev node
# (`arkiv-reth node --dev`) listening on 0.0.0.0 — for local development and
# testcontainers-based tests in other projects. Override CMD to change flags;
# `arkiv-cli` is on PATH for `docker exec`.

# ---- chef ----
# This stage, the planner stage and the cook step below are byte-identical
# across the arkiv-reth, arkiv-committer and arkiv-reth-dev Dockerfiles, so all
# builds resolve to the same layer digests and share one dependency
# compilation. Edit them in lockstep.
FROM rust:1.94-slim-bookworm AS chef
WORKDIR /build

# Native deps for the reth/alloy stack: libclang for bindgen (reth-mdbx-sys),
# plus clang/cmake/git/build-essential (also covers zstd-sys' bundled libzstd).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config libclang-dev clang cmake git build-essential \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked --version 0.1.77

# ---- planner ----
# `cargo chef prepare` distills the workspace manifests into recipe.json, which
# is byte-identical across source edits that leave Cargo.toml/Cargo.lock alone.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder ----
# Cooking the recipe compiles the whole workspace's third-party dependency tree
# into an ordinary layer; only manifest, lockfile or toolchain changes
# invalidate it. The workspace crates then build on top of that target/ dir.
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin arkiv-reth --bin arkiv-cli

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

COPY --from=builder /build/target/release/arkiv-reth /usr/local/bin/arkiv-reth
COPY --from=builder /build/target/release/arkiv-cli /usr/local/bin/arkiv-cli

USER arkiv
WORKDIR /home/arkiv
# EL JSON-RPC / WS, Engine API, p2p
EXPOSE 8545 8546 8551 30303 30303/udp
ENTRYPOINT ["arkiv-reth"]
# `arkiv` must NOT appear in --http.api/--ws.api: reth rejects unknown module
# names; the arkiv_* namespace is merged onto both transports at launch.
# 250ms block time matches the harness default so time-based behavior advances
# without traffic.
CMD ["node", "--dev", "--dev.block-time", "250ms", \
     "--http", "--http.addr", "0.0.0.0", "--http.api", "eth,net,web3,txpool", \
     "--ws", "--ws.addr", "0.0.0.0", "--ws.api", "eth,net,web3,txpool", \
     "--datadir", "/data"]
