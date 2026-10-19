# syntax=docker/dockerfile:1.7
#
# The dev image (`arkiv-reth` + `arkiv-cli`, as arkiv-reth-dev.Dockerfile) from
# binaries already compiled outside Docker -- for a prefill run off a branch
# CI does not publish, without recompiling the reth tree in a cold layer.
#
#   cargo build --profile release-fast --bin arkiv-reth --bin arkiv-cli
#   docker build -f docker/arkiv-reth-dev-prebuilt.Dockerfile \
#     --build-arg BASE_IMAGE=ubuntu:26.04 -t arkiv-reth-dev:local target/release-fast
#
# The context is the directory holding both binaries. BASE_IMAGE must carry a
# glibc at least as new as the build host's (see arkiv-reth-prebuilt.Dockerfile).
ARG BASE_IMAGE=ubuntu:24.04
FROM ${BASE_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 714 -m -s /usr/sbin/nologin arkiv \
    && mkdir -p /data && chown arkiv:arkiv /data

COPY --chmod=0755 arkiv-reth /usr/local/bin/arkiv-reth
COPY --chmod=0755 arkiv-cli /usr/local/bin/arkiv-cli

USER arkiv
WORKDIR /home/arkiv
EXPOSE 8545 8546 8551 30303 30303/udp
ENTRYPOINT ["arkiv-reth"]
# As arkiv-reth-dev.Dockerfile: `arkiv` must not appear in --http.api/--ws.api.
CMD ["node", "--dev", "--dev.block-time", "250ms", \
     "--http", "--http.addr", "0.0.0.0", "--http.api", "eth,net,web3,txpool", \
     "--ws", "--ws.addr", "0.0.0.0", "--ws.api", "eth,net,web3,txpool", \
     "--datadir", "/data"]
