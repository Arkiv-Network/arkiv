#!/usr/bin/env bash
# Build the arkiv-node EL image and bring up the EL + CL devnet via kurtosis.
# Usage: scripts/kurtosis/up.sh [enclave]
set -euo pipefail

ENCLAVE="${1:-arkiv-harness}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
cd "$REPO_ROOT"

# Repo-local docker build cache. buildx exports the layer/build cache here so a
# fresh checkout (or CI runner) can prime from it. The cargo type=cache mounts
# in the Dockerfile stay inside the builder instance, not in this dir.
CACHE_DIR=".docker/caches"
BUILDER="arkiv-harness"
mkdir -p "$CACHE_DIR"

# A docker-container builder is required for type=local cache export; the
# default "docker" driver does not support it.
if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  docker buildx create --name "$BUILDER" --driver docker-container >/dev/null
fi

echo "==> docker buildx build arkiv-node:dev (cache: $CACHE_DIR)"
docker buildx build \
  --builder "$BUILDER" \
  --tag arkiv-node:dev \
  --file docker/arkiv-node.Dockerfile \
  --cache-from "type=local,src=$CACHE_DIR" \
  --cache-to "type=local,dest=$CACHE_DIR,mode=max" \
  --load \
  .

echo "==> kurtosis run --enclave $ENCLAVE"
kurtosis run --enclave "$ENCLAVE" \
  github.com/ethpandaops/ethereum-package \
  --args-file kurtosis/devnet.yaml
