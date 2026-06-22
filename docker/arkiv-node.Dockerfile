# syntax=docker/dockerfile:1.7
#
# Builds the dummy arkiv-node (vanilla reth) as a static musl binary on an
# alpine builder, then ships it on a bare alpine runtime.

# ---- builder: native musl on alpine ----
FROM rust:1.94-alpine AS builder
WORKDIR /build

# C toolchain + headers reth's native deps (libmdbx, etc.) need under musl.
RUN apk add --no-cache build-base clang lld cmake linux-headers git pkgconfig

COPY . .

# target/ and the cargo caches are cache mounts (unmounted after RUN), so copy
# the finished binary to a real layer path for stage 2 to COPY --from.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin arkiv-node \
    && cp target/release/arkiv-node /build/arkiv-node

# ---- runtime: bare alpine ----
FROM alpine:3.20 AS runtime
RUN apk add --no-cache ca-certificates \
    && adduser -D -u 714 arkiv \
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
