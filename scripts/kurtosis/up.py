#!/usr/bin/env python3
"""Build the service images and bring up the devnet in one kurtosis enclave.

The Arkiv chain is run from the remote ethereum-package with the local
arkiv-reth execution client image and Lighthouse consensus client.

Usage: scripts/kurtosis/up.py [enclave]
"""

import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CACHE, BUILDER = ROOT / ".docker/caches", "arkiv-harness"
enclave = sys.argv[1] if len(sys.argv) > 1 else "arkiv-harness"


def log(message):
    """Print a flushed, timestamped step marker for local and CI runs."""
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    print(f"[{timestamp}] {message}", flush=True)


def run(*cmd):
    # Run every command from the repository root so relative paths in the
    # Dockerfile, Kurtosis args, and health check resolve consistently.
    log("$ " + " ".join(str(part) for part in cmd))
    subprocess.run(cmd, check=True, cwd=ROOT)


def buildx(tag, dockerfile):
    # Keep the local BuildKit cache between runs; compiling reth is expensive,
    # while the final image is loaded into the local Docker daemon for Kurtosis.
    ci_args = []
    cache_args = []
    if os.environ.get("CI"):
        # Limit memory use during the large release build. The hosted runner
        # cannot reuse a local cache, so do not export a duplicate cache there.
        ci_args = ["--build-arg", "CARGO_BUILD_JOBS=2"]
    else:
        cache_args = [
            "--cache-from",
            f"type=local,src={CACHE}",
            "--cache-to",
            f"type=local,dest={CACHE},mode=max",
        ]
    run(
        "docker", "buildx", "build", "--builder", BUILDER, "-t", tag,
        "-f", dockerfile,
        *ci_args,
        *cache_args,
        "--load", ".",
    )  # fmt: skip


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


CACHE.mkdir(parents=True, exist_ok=True)
# Create the named builder lazily so the first invocation works on a new host.
if subprocess.run(["docker", "buildx", "inspect", BUILDER], capture_output=True).returncode:
    run("docker", "buildx", "create", "--name", BUILDER, "--driver", "docker-container")

log(f"START Arkiv Kurtosis bring-up (enclave={enclave})")
log(f"Docker version: {subprocess.check_output(['docker', '--version'], text=True).strip()}")
log(
    f"Kurtosis version: {subprocess.check_output(['kurtosis', 'version'], text=True).splitlines()[0]}"
)
log("==> build images")
buildx("arkiv-reth:dev", "docker/arkiv-reth.Dockerfile")

# Start the two-node network only after the image is available to Kurtosis.
remove_stopped_enclave(enclave)
log(f"==> Arkiv chain (ethereum-package) -> enclave {enclave}")
run(
    "kurtosis", "run", "--enclave", enclave,
    "github.com/ethpandaops/ethereum-package",
    "--args-file", "./kurtosis/arkiv-chain.yaml",
)  # fmt: skip

log("==> check sequencer/follower health")
# Fail the bring-up if either RPC is unavailable, block production is idle, or
# the follower does not catch up with the sequencer.
run(sys.executable, "scripts/kurtosis/check.py", "--enclave", enclave)
log("DONE Arkiv Kurtosis bring-up")
