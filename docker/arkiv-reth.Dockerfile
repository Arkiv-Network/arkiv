# syntax=docker/dockerfile:1.7
#
# Builds the dummy arkiv-reth (vanilla reth) on a glibc builder and ships it on
# debian-slim. Alpine/musl was attempted first but reth-tasks does not compile
# against musl's sched_param (extra sched_ss_* fields); glibc is reth's
# supported platform, so we use it.

# ---- chef ----
# This stage contains the workspace dependency build used by arkiv-reth.
FROM rust:1.97-slim-bookworm AS chef
WORKDIR /build

# Native deps for the reth/alloy stack: libclang for bindgen (reth-mdbx-sys),
# plus clang/cmake/git/build-essential.
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
# BuildKit fills TARGETARCH in from the platform being built for.
ARG TARGETARCH
COPY --from=planner /build/recipe.json recipe.json
# jemalloc, which reth links in, fixes its page size at compile time — and both
# the cook and the build step compile it. arm64 is built for 64 KiB pages: such
# a build also runs on hosts with 4 KiB and 16 KiB pages, while one built for
# 4 KiB pages aborts at startup on a larger-page host.
RUN if [ "$TARGETARCH" = "arm64" ]; then export JEMALLOC_SYS_WITH_LG_PAGE=16; fi; \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN if [ "$TARGETARCH" = "arm64" ]; then export JEMALLOC_SYS_WITH_LG_PAGE=16; fi; \
    cargo build --release --locked --bin arkiv-reth

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

COPY --from=builder /build/target/release/arkiv-reth /usr/local/bin/arkiv-reth-bin
# ethereum-package currently supplies ENR bootnodes to the EL. Reth v2.5.0
# accepts enode URLs for --bootnodes, but not ENRs; remove incompatible values
# while leaving consensus-layer bootnodes untouched.
RUN <<'EOF'
cat > /usr/local/bin/arkiv-reth <<'SCRIPT'
#!/bin/sh
set -eu
args=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --bootnodes=enr:*|--bootnodes=)
            shift
            continue
            ;;
    esac
    if [ "$1" = "--bootnodes" ] && [ "$#" -ge 2 ]; then
        case "$2" in
            enr:*|"") ;;
            *) args="$args --bootnodes $2" ;;
        esac
        shift 2
        continue
    fi
    args="$args $1"
    shift
done
eval "exec /usr/local/bin/arkiv-reth-bin $args"
SCRIPT
chmod +x /usr/local/bin/arkiv-reth
EOF
# ethpandaops/ethereum-package's reth launcher runs a binary named `reth`;
# alias it so the image drops into a kurtosis `el_type: reth` participant.
RUN ln -sf /usr/local/bin/arkiv-reth /usr/local/bin/reth

USER arkiv
WORKDIR /home/arkiv
# EL JSON-RPC / WS, Engine API, p2p
EXPOSE 8545 8546 8551 30303 30303/udp
ENTRYPOINT ["arkiv-reth"]
