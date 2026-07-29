# syntax=docker/dockerfile:1.7
#
# Builds the dummy arkiv-node (vanilla reth) on a glibc builder and ships it on
# debian-slim. Alpine/musl was attempted first but reth-tasks does not compile
# against musl's sched_param (extra sched_ss_* fields); glibc is reth's
# supported platform, so we use it.

# ---- chef ----
# This stage, the planner stage and the cook step below are byte-identical
# across the arkiv-node and arkiv-committer Dockerfiles, so both builds resolve
# to the same layer digests and share one dependency compilation. Edit them in
# lockstep.
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
RUN cargo build --release --locked --bin arkiv-node

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

COPY --from=builder /build/target/release/arkiv-node /usr/local/bin/arkiv-node
# ethpandaops/ethereum-package's reth launcher runs a binary named `reth`;
# alias it so the image drops into a kurtosis `el_type: reth` participant.
RUN ln -sf /usr/local/bin/arkiv-node /usr/local/bin/reth

USER arkiv
WORKDIR /home/arkiv
# EL JSON-RPC / WS, Engine API, p2p
EXPOSE 8545 8546 8551 30303 30303/udp
ENTRYPOINT ["arkiv-node"]
