#!/usr/bin/env python3
"""Bring up the devnet in one kurtosis enclave and check its health.

The Arkiv chain is run from the remote ethereum-package with the local
arkiv-reth execution client image and Lighthouse consensus client.

This script builds nothing: `just kurtosis_up` (or `just kurtosis_up_prebuilt`)
builds the arkiv-reth:dev and arkiv-genesis-generator:dev images first, then
runs it.

Usage: scripts/kurtosis/up.py [enclave]
"""

import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ARGS_FILE = ROOT / "kurtosis/arkiv-chain.yaml"
# Pinned: the genesis generator image wraps the package's own default
# generator, so the two must move together (kurtosis/genesis-generator).
PACKAGE = "github.com/ethpandaops/ethereum-package@903982a05382b9be07fca48579c75384df0a119c"
# The local images kurtosis/arkiv-chain.yaml names.
IMAGES = ("arkiv-reth:dev", "arkiv-genesis-generator:dev")
enclave = sys.argv[1] if len(sys.argv) > 1 else "arkiv-harness"


def log(message):
    """Print a flushed, timestamped step marker for local and CI runs."""
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    print(f"[{timestamp}] {message}", flush=True)


def run(*cmd):
    # Run every command from the repository root so relative paths in the
    # Kurtosis args and health check resolve consistently.
    log("$ " + " ".join(str(part) for part in cmd))
    subprocess.run(cmd, check=True, cwd=ROOT)


def require_images():
    # Kurtosis would try to pull a missing local tag from Docker Hub and fail
    # deep in the package's output; say what is missing up front instead.
    missing = [
        image
        for image in IMAGES
        if subprocess.run(["docker", "image", "inspect", image], capture_output=True).returncode
    ]
    if missing:
        sys.exit(f"missing image(s) {', '.join(missing)}: build them with `just kurtosis_up`")


def remove_stopped_enclave(name):
    # Kurtosis cannot append to an enclave whose containers are stopped. Remove
    # only a stopped enclave; a running enclave remains available for inspection
    # and independent health checks.
    result = subprocess.run(
        ["kurtosis", "enclave", "ls"],
        check=True,
        capture_output=True,
        text=True,
    )
    for line in result.stdout.splitlines():
        fields = line.split()
        if len(fields) >= 3 and fields[1] == name and fields[2] == "STOPPED":
            log(f"Removing stopped Kurtosis enclave: {name}")
            run("kurtosis", "enclave", "rm", "-f", name)
            return


require_images()
log(f"START Arkiv Kurtosis bring-up (enclave={enclave})")
log(f"Docker version: {subprocess.check_output(['docker', '--version'], text=True).strip()}")
log(
    f"Kurtosis version: {subprocess.check_output(['kurtosis', 'version'], text=True).splitlines()[0]}"
)

remove_stopped_enclave(enclave)
log(f"==> Arkiv chain (ethereum-package) -> enclave {enclave}")
run(
    "kurtosis", "run", "--enclave", enclave,
    PACKAGE,
    "--args-file", str(ARGS_FILE.relative_to(ROOT)),
)  # fmt: skip

log("==> check sequencer/follower health")
# Fail the bring-up if either RPC is unavailable, block production is idle, or
# the follower does not catch up with the sequencer.
run(sys.executable, "scripts/kurtosis/check.py", "--enclave", enclave)
log("DONE Arkiv Kurtosis bring-up")
