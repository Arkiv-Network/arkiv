# A light dev container for arkiv: the pinned Rust toolchain plus the system
# packages the full workspace build needs (reth's bindgen and C deps). Nothing
# else — no Docker socket, no Kurtosis, no Foundry. See devcontainer.json.
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# lld is the linker on aarch64; devcontainer.json explains why.
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev pkg-config cmake zstd lld \
    && rm -rf /var/lib/apt/lists/*

# Install the toolchain rust-toolchain.toml pins (channel and components) into
# the image, so the first `cargo` call in the container does not download it.
# rust-src is added on top: rust-analyzer needs the standard library's source
# to resolve std, and CI, which shares the toolchain file, has no use for it.
# Installing as root leaves the new toolchain in group root; hand it to the
# rustlang group the vscode user is in, so rustup works without sudo later.
COPY rust-toolchain.toml /tmp/toolchain/rust-toolchain.toml
RUN cd /tmp/toolchain \
    && rustup toolchain install \
    && rustup component add rust-src \
    && rm -rf /tmp/toolchain \
    && chgrp -R rustlang "$RUSTUP_HOME" "$CARGO_HOME" \
    && chmod -R g+w "$RUSTUP_HOME" "$CARGO_HOME"
