#!/usr/bin/env python3
"""Build the service images and bring up the devnet in one kurtosis enclave.

The Arkiv chain is run from the remote ethereum-package with the local
arkiv-reth execution client image and Lighthouse consensus client.

Set ARKIV_RETH_BINARY to an already-compiled arkiv-reth to package that
binary instead of compiling the workspace inside Docker. CI points it at the
binary the build lane produced; leaving it unset keeps the from-source build.

Set ARKIV_SEED_COUNT to a positive number to start the chain with that many
entities already in its genesis state: `arkiv-cli seed-genesis` builds them
(ARKIV_SEED_PAYLOAD_SIZE bytes of payload each, default 512) and the accounts
go to ethereum-package as `additional_preloaded_contracts`, so the sequencer
and the follower both compute the same seeded genesis. The accounts travel in
the package arguments, which Kurtosis caps at 4 MiB: about 1000 entities of
512 B, or 2000 of 256 B; bigger seeds take the genesis-file or init-state
route (see the README) rather than ethereum-package. ARKIV_CLI_BINARY names
the arkiv-cli to use (default: next to ARKIV_RETH_BINARY, else a cargo build).
The manifest lands in kurtosis/.seed/manifest.json for the integration tests.

Usage: scripts/kurtosis/up.py [enclave]
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CACHE, BUILDER = ROOT / ".docker/caches", "arkiv-harness"
ARGS_FILE = ROOT / "kurtosis/arkiv-chain.yaml"
SEED_DIR = ROOT / "kurtosis/.seed"
# The first account ethereum-package prefunds in every genesis it generates
# (m/44'/60'/0'/0/0 of its well-known mnemonic) — the same key the
# integration tests spend from, so it can also operate on seeded entities.
SEED_OWNER = "0x8943545177806ED17B9F23F0a21ee5948eCaa776"
# ethereum-package's default network id, unless the args file says otherwise.
DEFAULT_NETWORK_ID = 3151908
enclave = sys.argv[1] if len(sys.argv) > 1 else "arkiv-harness"
prebuilt = os.environ.get("ARKIV_RETH_BINARY")
seed_count = int(os.environ.get("ARKIV_SEED_COUNT", "0"))
seed_payload_size = int(os.environ.get("ARKIV_SEED_PAYLOAD_SIZE", "512"))
# Kurtosis sends the package arguments in one gRPC message and refuses one
# over 4 MiB ("received message larger than max"); the seeded alloc is most of
# that message. Checked before `kurtosis run`, for a message that names the fix.
KURTOSIS_ARGS_LIMIT = 4 * 1024 * 1024


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


def package_prebuilt(tag, binary):
    # No builder, no cache: the image is one COPY over a distro base, and the
    # context holds nothing but the binary, so this takes a couple of seconds.
    binary = Path(binary).resolve()
    if not binary.is_file():
        sys.exit(f"ARKIV_RETH_BINARY does not point at a file: {binary}")
    # The base image must carry a glibc at least as new as the build host's;
    # ARKIV_PREBUILT_BASE_IMAGE overrides the Dockerfile's default for hosts
    # newer than the CI runner.
    base_args = []
    base_image = os.environ.get("ARKIV_PREBUILT_BASE_IMAGE")
    if base_image:
        base_args = ["--build-arg", f"BASE_IMAGE={base_image}"]
    with tempfile.TemporaryDirectory() as context:
        shutil.copy2(binary, Path(context) / "arkiv-reth")
        run(
            "docker", "build", "-t", tag,
            "-f", str(ROOT / "docker/arkiv-reth-prebuilt.Dockerfile"),
            *base_args,
            context,
        )  # fmt: skip


def arkiv_cli():
    """The arkiv-cli binary to seed with: named, next to the prebuilt node,
    an existing build, or a fresh cargo build (dev-fast, like CI)."""
    named = os.environ.get("ARKIV_CLI_BINARY")
    if named:
        return Path(named).resolve()
    candidates = []
    if prebuilt:
        candidates.append(Path(prebuilt).resolve().with_name("arkiv-cli"))
    candidates += [ROOT / "target/dev-fast/arkiv-cli", ROOT / "target/debug/arkiv-cli"]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    run("cargo", "build", "--profile", "dev-fast", "--bin", "arkiv-cli")
    return ROOT / "target/dev-fast/arkiv-cli"


def seeded_args_file(count, payload_size):
    """Build the seed and write an args file carrying it as preloaded
    contracts. Returns the path of that args file."""
    import yaml  # PyYAML: on the CI runner and in most dev Pythons

    with open(ARGS_FILE) as handle:
        args = yaml.safe_load(handle)
    network_params = args.setdefault("network_params", {})
    network_id = int(network_params.get("network_id", DEFAULT_NETWORK_ID))

    SEED_DIR.mkdir(parents=True, exist_ok=True)
    alloc_path, manifest_path = SEED_DIR / "alloc.json", SEED_DIR / "manifest.json"
    owners = os.environ.get("ARKIV_SEED_OWNERS", SEED_OWNER).split(",")
    command = [
        str(arkiv_cli()), "seed-genesis",
        "--format", "alloc",
        "--count", str(count),
        "--payload-size", str(payload_size),
        "--chain-id", str(network_id),
        "--out", str(alloc_path),
        "--manifest-out", str(manifest_path),
    ]  # fmt: skip
    for owner in owners:
        command += ["--owner", owner.strip()]
    run(*command)

    with open(alloc_path) as handle:
        network_params["additional_preloaded_contracts"] = json.load(handle)
    seeded_args = SEED_DIR / "arkiv-chain.seeded.yaml"
    with open(seeded_args, "w") as handle:
        handle.write(
            "# Generated by scripts/kurtosis/up.py from kurtosis/arkiv-chain.yaml "
            f"with {count} seeded entities. Do not edit.\n"
        )
        yaml.safe_dump(args, handle, sort_keys=False)
    size = seeded_args.stat().st_size
    if size >= KURTOSIS_ARGS_LIMIT:
        sys.exit(
            f"the seeded args file is {size / 2**20:.1f} MiB and Kurtosis refuses "
            f"package arguments over {KURTOSIS_ARGS_LIMIT // 2**20} MiB: lower "
            "ARKIV_SEED_COUNT or ARKIV_SEED_PAYLOAD_SIZE, or seed a bigger state "
            "through `arkiv-reth init-state` instead of ethereum-package"
        )
    log(f"seeded {count} entities; manifest at {manifest_path} ({size / 2**20:.1f} MiB of args)")
    return seeded_args


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


if not prebuilt:
    CACHE.mkdir(parents=True, exist_ok=True)
    # Create the named builder lazily so the first invocation works on a new host.
    if subprocess.run(["docker", "buildx", "inspect", BUILDER], capture_output=True).returncode:
        run("docker", "buildx", "create", "--name", BUILDER, "--driver", "docker-container")

log(f"START Arkiv Kurtosis bring-up (enclave={enclave})")
log(f"Docker version: {subprocess.check_output(['docker', '--version'], text=True).strip()}")
log(
    f"Kurtosis version: {subprocess.check_output(['kurtosis', 'version'], text=True).splitlines()[0]}"
)
if prebuilt:
    log(f"==> package prebuilt arkiv-reth ({prebuilt})")
    package_prebuilt("arkiv-reth:dev", prebuilt)
else:
    log("==> build images")
    buildx("arkiv-reth:dev", "docker/arkiv-reth.Dockerfile")

args_file = ARGS_FILE
if seed_count > 0:
    log(f"==> seed genesis ({seed_count} entities x {seed_payload_size} B)")
    args_file = seeded_args_file(seed_count, seed_payload_size)

# Start the two-node network only after the image is available to Kurtosis.
remove_stopped_enclave(enclave)
log(f"==> Arkiv chain (ethereum-package) -> enclave {enclave}")
run(
    "kurtosis", "run", "--enclave", enclave,
    "github.com/ethpandaops/ethereum-package",
    "--args-file", str(args_file.relative_to(ROOT)),
)  # fmt: skip

log("==> check sequencer/follower health")
# Fail the bring-up if either RPC is unavailable, block production is idle, or
# the follower does not catch up with the sequencer.
run(sys.executable, "scripts/kurtosis/check.py", "--enclave", enclave)
log("DONE Arkiv Kurtosis bring-up")
