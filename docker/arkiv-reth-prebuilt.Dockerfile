# syntax=docker/dockerfile:1.7
#
# The same runtime image as arkiv-reth.Dockerfile, but with no builder stage:
# it packages an `arkiv-reth` binary that was already compiled outside Docker.
# CI builds that binary once from the shared build cache, so the Kurtosis smoke
# test no longer recompiles the reth tree in a cold BuildKit layer.
#
# Build context must contain the binary; pass its path with
# `--build-arg BINARY=<path relative to the context>`.

# Not debian:bookworm-slim, unlike the from-source image. The binary comes from
# the GitHub runner (Ubuntu 24.04, glibc 2.39) and bookworm only has 2.36, so a
# dynamically linked reth built there will not start on it. Matching the
# builder's distribution keeps that contract simple.
FROM ubuntu:24.04 AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

ARG BINARY=arkiv-reth
COPY ${BINARY} /usr/local/bin/arkiv-reth-bin
RUN chmod 0755 /usr/local/bin/arkiv-reth-bin

# ethereum-package currently supplies ENR bootnodes to the EL. Reth v2.5.0
# accepts enode URLs for --bootnodes, but not ENRs; remove incompatible values
# while leaving consensus-layer bootnodes untouched. Kept byte-identical with
# the wrapper in arkiv-reth.Dockerfile — edit them in lockstep.
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
