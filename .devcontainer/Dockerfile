# A light dev container for arkiv: the pinned Rust toolchain plus the system
# packages the full workspace build needs (reth's bindgen and C deps), plus
# what CI's tests run with: cargo-nextest and a Valkey server. Nothing else —
# no Docker socket, no Kurtosis, no Foundry. See devcontainer.json.
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# lld is the linker on aarch64; devcontainer.json explains why.
# valkey-server serves the `--ignored` store tests, as the Valkey service does
# in CI. Bookworm has it only in backports, which also carries CI's major
# version (8); `-t` limits backports to that one package.
RUN echo 'deb http://deb.debian.org/debian bookworm-backports main' \
        > /etc/apt/sources.list.d/bookworm-backports.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev pkg-config cmake zstd lld \
    && apt-get install -y --no-install-recommends -t bookworm-backports valkey-server \
    && rm -rf /var/lib/apt/lists/*

# cargo-nextest, the test runner CI uses (`cargo nextest run --workspace`).
# Prebuilt, and the latest release, as CI's install action takes it.
RUN case "$(dpkg --print-architecture)" in \
        amd64) platform=linux ;; \
        arm64) platform=linux-arm ;; \
        *) echo "no cargo-nextest build for $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac \
    && curl -LsSf "https://get.nexte.st/latest/$platform" | tar zxf - -C "$CARGO_HOME/bin"

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
