#!/usr/bin/env bash
# Tear down the kurtosis enclave.
# Usage: scripts/kurtosis/down.sh [enclave]
set -euo pipefail

ENCLAVE="${1:-arkiv-harness}"
kurtosis enclave rm -f "$ENCLAVE"
