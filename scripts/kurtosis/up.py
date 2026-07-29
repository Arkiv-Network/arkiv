#!/usr/bin/env python3
"""Build the service images and bring up the devnet in one kurtosis enclave.

Additive runs into the same enclave (kurtosis can't import a remote package from
a local one): the Arkiv chain (remote ethereum-package = arkiv-node EL +
lighthouse CL), the base chain (local package = plain reth --dev), and the
committer (local package = DA stub that connects the two).

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


def buildx(tag, dockerfile):
    run(
        "docker", "buildx", "build", "--builder", BUILDER, "-t", tag,
        "-f", dockerfile,
        "--cache-from", f"type=local,src={CACHE}",
        "--cache-to", f"type=local,dest={CACHE},mode=max",
        "--load", ".",
    )  # fmt: skip


def run_local_pkg(name):
    # kurtosis roots a local package's archive at the git repo root, which breaks
    # a package in a subdir; run the self-contained package from a neutral temp dir.
    pkg = ROOT / "kurtosis" / name
    with tempfile.TemporaryDirectory() as tmp:
        for f in ("kurtosis.yml", "main.star"):
            shutil.copy(pkg / f, Path(tmp) / f)
        run("kurtosis", "run", "--enclave", enclave, tmp)


CACHE.mkdir(parents=True, exist_ok=True)
if subprocess.run(["docker", "buildx", "inspect", BUILDER], capture_output=True).returncode:
    run("docker", "buildx", "create", "--name", BUILDER, "--driver", "docker-container")

print("==> build images")
buildx("arkiv-reth:dev", "docker/arkiv-reth.Dockerfile")
buildx("arkiv-committer:dev", "docker/arkiv-committer.Dockerfile")

print(f"==> Arkiv chain (ethereum-package) -> enclave {enclave}")
run(
    "kurtosis", "run", "--enclave", enclave,
    "github.com/ethpandaops/ethereum-package",
    "--args-file", "./kurtosis/arkiv-chain.yaml",
)  # fmt: skip

print(f"==> base chain (reth --dev) -> enclave {enclave}")
run_local_pkg("base-chain")

print(f"==> committer (DA stub) -> enclave {enclave}")
run_local_pkg("committer")
