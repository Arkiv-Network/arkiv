#!/usr/bin/env bash
# Build the arkiv-node EL image and bring up the EL + CL devnet via kurtosis.
# Usage: scripts/kurtosis/up.sh [enclave]
set -euo pipefail

ENCLAVE="${1:-arkiv-harness}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
cd "$REPO_ROOT"

echo "==> docker build arkiv-node:dev"
docker build -t arkiv-node:dev -f docker/arkiv-node.Dockerfile .

echo "==> kurtosis run --enclave $ENCLAVE"
kurtosis run --enclave "$ENCLAVE" \
  github.com/ethpandaops/ethereum-package \
  --args-file kurtosis/devnet.yaml
