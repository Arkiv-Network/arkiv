# The genesis generator ethereum-package runs, made to agree with arkiv-reth
# on block 0.
#
# A genesis file with Arkiv's GolemDB settings gives block 0 GolemDB's state
# root, so its hash is not the one the stock generator derives. entrypoint.sh
# adds the settings to the generated genesis.json, has arkiv-reth build block 0
# from it, and gives that block to the CL genesis as its shadow-fork block.
#
# Built by scripts/kurtosis/up.py after arkiv-reth:dev, whose binary it takes.
# Keep the base tag equal to the pinned ethereum-package's default generator
# image (src/package_io/constants.star).
FROM arkiv-reth:dev AS node

FROM ethpandaops/ethereum-genesis-generator:6.2.7
COPY --from=node /usr/local/bin/arkiv-reth /usr/local/bin/arkiv-reth
RUN mv /work/entrypoint.sh /work/entrypoint-stock.sh
COPY entrypoint.sh /work/entrypoint.sh
