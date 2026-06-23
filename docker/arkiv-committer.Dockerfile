# syntax=docker/dockerfile:1.7
#
# Builds the arkiv-committer service on a glibc builder and ships it on
# debian-slim — same toolchain as docker/arkiv-node.Dockerfile (reth's deps are
# pulled transitively via arkiv-da, so the native build deps are identical).

# ---- builder ----
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build

# Native deps for the reth/alloy transitive stack: libclang for bindgen, plus
# clang/cmake/git/build-essential (also covers zstd-sys' bundled libzstd).
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
    cargo build --release --locked --bin arkiv-committer \
    && cp target/release/arkiv-committer /build/arkiv-committer

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 715 -m -s /usr/sbin/nologin arkiv

COPY --from=builder /build/arkiv-committer /usr/local/bin/arkiv-committer

USER arkiv
WORKDIR /home/arkiv
ENTRYPOINT ["arkiv-committer"]
