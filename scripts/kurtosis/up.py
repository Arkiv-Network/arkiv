#!/usr/bin/env python3
"""Build the arkiv-node EL image and bring up the devnet in one kurtosis enclave.

Two additive runs into the same enclave (kurtosis can't import a remote package
from a local one): the Arkiv chain (remote ethereum-package = arkiv-node EL +
lighthouse CL), then the base chain (local package = plain reth --dev).

Usage: scripts/kurtosis/up.py [enclave]
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CACHE, BUILDER = ROOT / ".docker/caches", "arkiv-harness"
enclave = sys.argv[1] if len(sys.argv) > 1 else "arkiv-harness"


def run(*cmd):
    print("$", *cmd, flush=True)
    subprocess.run(cmd, check=True, cwd=ROOT)


CACHE.mkdir(parents=True, exist_ok=True)
if subprocess.run(["docker", "buildx", "inspect", BUILDER], capture_output=True).returncode:
    run("docker", "buildx", "create", "--name", BUILDER, "--driver", "docker-container")

print("==> build arkiv-node:dev image")
run(
    "docker",
    "buildx",
    "build",
    "--builder",
    BUILDER,
    "-t",
    "arkiv-node:dev",
    "-f",
    "docker/arkiv-node.Dockerfile",
    "--cache-from",
    f"type=local,src={CACHE}",
    "--cache-to",
    f"type=local,dest={CACHE},mode=max",
    "--load",
    ".",
)

print(f"==> Arkiv chain (ethereum-package) -> enclave {enclave}")
run(
    "kurtosis",
    "run",
    "--enclave",
    enclave,
    "github.com/ethpandaops/ethereum-package",
    "--args-file",
    "./kurtosis/arkiv-chain.yaml",
)

# kurtosis roots a local package's archive at the git repo root, which breaks a
# package in a subdir; run the self-contained base-chain from a neutral temp dir.
print(f"==> base chain (reth --dev) -> enclave {enclave}")
with tempfile.TemporaryDirectory() as tmp:
    for f in ("kurtosis.yml", "main.star"):
        shutil.copy(ROOT / "kurtosis/base-chain" / f, Path(tmp) / f)
    run("kurtosis", "run", "--enclave", enclave, tmp)
