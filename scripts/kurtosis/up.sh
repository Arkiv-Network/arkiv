#!/usr/bin/env bash
# Build the arkiv-node EL image and bring up the devnet via kurtosis.
#
# Both chains run into ONE enclave via two additive kurtosis runs:
#   1. the Arkiv chain — remote ethereum-package (arkiv-node EL + lighthouse CL)
#   2. the base chain  — a local package adding a plain reth --dev service
# (kurtosis can't import a remote package from a local one, hence two runs.)
#
# Usage: scripts/kurtosis/up.sh [enclave]
set -euo pipefail

ENCLAVE="${1:-arkiv-harness}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
cd "$REPO_ROOT"

# Repo-local docker build cache (layer cache exported here for fresh checkouts).
CACHE_DIR=".docker/caches"
BUILDER="arkiv-harness"
mkdir -p "$CACHE_DIR"
if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  docker buildx create --name "$BUILDER" --driver docker-container >/dev/null
fi

echo "==> docker buildx build arkiv-node:dev"
docker buildx build --builder "$BUILDER" -t arkiv-node:dev \
  -f docker/arkiv-node.Dockerfile \
  --cache-from "type=local,src=$CACHE_DIR" \
  --cache-to "type=local,dest=$CACHE_DIR,mode=max" \
  --load .

echo "==> Arkiv chain (ethereum-package) -> enclave $ENCLAVE"
kurtosis run --enclave "$ENCLAVE" \
  github.com/ethpandaops/ethereum-package \
  --args-file ./kurtosis/arkiv-chain.yaml

echo "==> base chain -> enclave $ENCLAVE"
kurtosis run --enclave "$ENCLAVE" ./kurtosis/base-chain
