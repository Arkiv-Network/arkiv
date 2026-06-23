# syntax=docker/dockerfile:1.7
#
# Builds the dummy arkiv-node (vanilla reth) on a glibc builder and ships it on
# debian-slim. Alpine/musl was attempted first but reth-tasks does not compile
# against musl's sched_param (extra sched_ss_* fields); glibc is reth's
# supported platform, so we use it.

# ---- builder ----
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build

# reth native deps: libclang for bindgen (reth-mdbx-sys), plus clang/cmake/git.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config libclang-dev clang cmake git build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# target/ and the cargo caches are cache mounts (unmounted after RUN), so copy
# the finished binary to a real layer path for stage 2 to COPY --from.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin arkiv-node \
    && cp target/release/arkiv-node /build/arkiv-node

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

COPY --from=builder /build/arkiv-node /usr/local/bin/arkiv-node
# ethpandaops/ethereum-package's reth launcher runs a binary named `reth`;
# alias it so the image drops into a kurtosis `el_type: reth` participant.
RUN ln -sf /usr/local/bin/arkiv-node /usr/local/bin/reth

USER arkiv
WORKDIR /home/arkiv
# EL JSON-RPC / WS, Engine API, p2p
EXPOSE 8545 8546 8551 30303 30303/udp
ENTRYPOINT ["arkiv-node"]
