# syntax=docker/dockerfile:1.7
#
# Dev image: `arkiv-node` + `arkiv-cli`, defaulting to an auto-sealing dev node
# (`arkiv-node node --dev`) listening on 0.0.0.0 — for local development and
# testcontainers-based tests in other projects. Override CMD to change flags;
# `arkiv-cli` is on PATH for `docker exec`.

# ---- builder ----
# NOTE: keep this stage byte-identical with docker/arkiv-node.Dockerfile so
# BuildKit serves it from cache when both images are built on one builder.
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build

# reth native deps: libclang for bindgen (reth-mdbx-sys), plus clang/cmake/git.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config libclang-dev clang cmake git build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# target/ and the cargo caches are cache mounts (unmounted after RUN), so copy
# the finished binaries to a real layer path for stage 2 to COPY --from.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin arkiv-node --bin arkiv-cli \
    && cp target/release/arkiv-node target/release/arkiv-cli /build/

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

COPY --from=builder /build/arkiv-node /usr/local/bin/arkiv-node
COPY --from=builder /build/arkiv-cli /usr/local/bin/arkiv-cli

USER arkiv
WORKDIR /home/arkiv
# EL JSON-RPC / WS, Engine API, p2p
EXPOSE 8545 8546 8551 30303 30303/udp
ENTRYPOINT ["arkiv-node"]
# `arkiv` must NOT appear in --http.api/--ws.api: reth rejects unknown module
# names; the arkiv_* namespace is merged onto both transports at launch.
# 250ms block time matches the harness/e2e default so time-based behavior
# (e.g. BTL expiry) advances without traffic.
CMD ["node", "--dev", "--dev.block-time", "250ms", \
     "--http", "--http.addr", "0.0.0.0", "--http.api", "eth,net,web3,txpool", \
     "--ws", "--ws.addr", "0.0.0.0", "--ws.api", "eth,net,web3,txpool", \
     "--datadir", "/data"]
