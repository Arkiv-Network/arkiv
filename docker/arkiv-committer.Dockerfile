# syntax=docker/dockerfile:1.7
#
# Builds the arkiv-committer service on a glibc builder and ships it on
# debian-slim — same toolchain as docker/arkiv-reth.Dockerfile (reth's deps are
# pulled transitively via arkiv-da, so the native build deps are identical).

# ---- chef ----
# This stage, the planner stage and the cook step below are byte-identical
# across the arkiv-reth and arkiv-committer Dockerfiles, so both builds resolve
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
RUN cargo build --release --locked --bin arkiv-committer

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 715 -m -s /usr/sbin/nologin arkiv

COPY --from=builder /build/target/release/arkiv-committer /usr/local/bin/arkiv-committer

USER arkiv
WORKDIR /home/arkiv
ENTRYPOINT ["arkiv-committer"]
