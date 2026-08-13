#!/usr/bin/env python3
"""Check the Arkiv Kurtosis network and its sequencer/follower RPCs.

Usage:
    scripts/kurtosis/check.py [--enclave NAME] [--sequencer-rpc URL]
        [--follower-rpc URL] [--wait SECONDS]

The default published ports match the first two EL RPC endpoints in
`kurtosis/arkiv-chain.yaml`: 32003 and 32010. The package allocates all EL
ports for participant 1 before allocating participant 2's ports.
"""

import argparse
from datetime import datetime, timezone
import json
import os
import subprocess
import sys
import time
from urllib.error import URLError
from urllib.request import Request, urlopen

DEFAULT_ENCLAVE = "arkiv-harness"
DEFAULT_SEQUENCER_RPC = "http://127.0.0.1:32003"
DEFAULT_FOLLOWER_RPC = "http://127.0.0.1:32010"
POLL_INTERVAL = 2.0
DIAGNOSTIC_ENCLAVE = None
USE_COLOR = False

COLORS = {
    "red": "\033[31m",
    "green": "\033[32m",
    "yellow": "\033[33m",
    "cyan": "\033[36m",
    "reset": "\033[0m",
}


def log(message, color=None):
    """Print a flushed, timestamped line that is easy to find in CI logs."""
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    line = f"[{timestamp}] {message}"
    if USE_COLOR and color:
        line = f"{COLORS[color]}{line}{COLORS['reset']}"
    print(line, flush=True)


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--enclave", default=DEFAULT_ENCLAVE)
    parser.add_argument("--sequencer-rpc", default=DEFAULT_SEQUENCER_RPC)
    parser.add_argument("--follower-rpc", default=DEFAULT_FOLLOWER_RPC)
    parser.add_argument(
        "--wait",
        type=float,
        default=30.0,
        help="seconds to wait for block production and follower catch-up (default: 30)",
    )
    parser.add_argument(
        "--max-follower-lag",
        type=int,
        default=2,
        help="maximum accepted follower lag in blocks (default: 2)",
    )
    parser.add_argument(
        "--samples",
        type=int,
        default=3,
        help="number of produced blocks to sample (default: 3)",
    )
    parser.add_argument(
        "--no-color",
        action="store_true",
        help="disable ANSI colors even when stdout is an interactive terminal",
    )
    return parser.parse_args()


def fail(message):
    # Keep all failures on stderr and use a non-zero exit code so this script
    # can be used directly from `up.py` and CI.
    line = f"FAIL: {message}"
    if USE_COLOR:
        line = f"{COLORS['red']}{line}{COLORS['reset']}"
    print(line, file=sys.stderr, flush=True)
    if DIAGNOSTIC_ENCLAVE:
        # Preserve the useful Kurtosis state in CI when a service dies before
        # its RPC becomes reachable.
        print(f"DIAGNOSTICS: kurtosis enclave inspect {DIAGNOSTIC_ENCLAVE}", flush=True)
        subprocess.run(["kurtosis", "enclave", "inspect", DIAGNOSTIC_ENCLAVE], check=False)
        print(f"DIAGNOSTICS: kurtosis service logs {DIAGNOSTIC_ENCLAVE} --all-services", flush=True)
        subprocess.run(
            [
                "kurtosis",
                "service",
                "logs",
                "--all-services",
                "--num",
                "120",
                DIAGNOSTIC_ENCLAVE,
            ],
            check=False,
        )
    raise SystemExit(1)


def check_enclave(name):
    # Query Kurtosis rather than Docker directly: an enclave can exist while
    # its individual services are still starting or have stopped.
    try:
        result = subprocess.run(
            ["kurtosis", "enclave", "ls"],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        fail("kurtosis CLI is not installed")
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout).strip()
        fail(f"cannot query Kurtosis; is Docker/Kurtosis engine running? {detail}")

    if name not in result.stdout:
        fail(f"Kurtosis enclave {name!r} is not running")
    log(f"OK: Kurtosis enclave {name} is running", "green")


def rpc(url, method, params=None):
    # The checker intentionally uses only Python's standard library so it can
    # run on a fresh development machine before the Rust workspace is built.
    payload = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}
    ).encode()
    request = Request(url, data=payload, headers={"Content-Type": "application/json"})
    try:
        with urlopen(request, timeout=5) as response:
            body = json.loads(response.read())
    except (OSError, URLError, json.JSONDecodeError) as error:
        fail(f"RPC {url} did not respond to {method}: {error}")
    if "error" in body:
        fail(f"RPC {url} returned an error for {method}: {body['error']}")
    return body.get("result")


def block_number(url):
    # Ethereum JSON-RPC encodes block numbers as hexadecimal quantities.
    result = rpc(url, "eth_blockNumber")
    try:
        return int(result, 16)
    except (TypeError, ValueError):
        fail(f"RPC {url} returned invalid eth_blockNumber result: {result!r}")


def block(url, number):
    """Fetch a full block so both nodes can be compared by hash."""
    result = rpc(url, "eth_getBlockByNumber", [hex(number), False])
    if not isinstance(result, dict):
        fail(f"RPC {url} returned no block for height {number}")
    if result.get("number") != hex(number) or not result.get("hash"):
        fail(f"RPC {url} returned malformed block at height {number}: {result!r}")
    return result


def check_rpc(url, role):
    # `net_version` confirms the endpoint belongs to the expected devnet;
    # `eth_blockNumber` confirms the EL RPC is actually serving chain state.
    chain_id = rpc(url, "net_version")
    number = block_number(url)
    log(f"OK: {role} RPC {url} responded (network={chain_id}, block={number})", "green")
    return chain_id, number


def check_network(args):
    # Capture both starting heights before polling. This lets us distinguish
    # a live-but-idle sequencer from a network that is producing blocks.
    sequencer_chain, sequencer_start = check_rpc(args.sequencer_rpc, "sequencer")
    follower_chain, follower_start = check_rpc(args.follower_rpc, "follower")
    if sequencer_chain != follower_chain:
        fail(
            f"nodes report different networks: sequencer={sequencer_chain}, follower={follower_chain}"
        )
    log(f"OK: sequencer and follower share network {sequencer_chain}", "green")

    # Validate the initial chain state before waiting for production.
    common_start = min(sequencer_start, follower_start)
    sequencer_block = block(args.sequencer_rpc, common_start)
    follower_block = block(args.follower_rpc, common_start)
    if sequencer_block["hash"] != follower_block["hash"]:
        fail(f"nodes disagree at initial common height {common_start}")
    log(f"OK: initial block hash matches at height {common_start}", "green")

    deadline = time.monotonic() + args.wait
    sequencer_latest = sequencer_start
    follower_latest = follower_start
    samples = 0

    while time.monotonic() < deadline and samples < args.samples:
        # Poll both nodes together so the reported lag describes the same
        # observation window rather than two unrelated snapshots.
        time.sleep(POLL_INTERVAL)
        sequencer_latest = block_number(args.sequencer_rpc)
        follower_latest = block_number(args.follower_rpc)
        if sequencer_latest > sequencer_start:
            lag = sequencer_latest - follower_latest
            log(
                f"INFO: sequencer advanced {sequencer_start} -> {sequencer_latest}; "
                f"follower={follower_latest} (lag={lag})",
                "cyan",
            )
            if lag > args.max_follower_lag:
                fail(f"follower lag exceeded limit: lag={lag}, allowed={args.max_follower_lag}")

            common_height = min(sequencer_latest, follower_latest)
            if common_height > 0:
                sequencer_block = block(args.sequencer_rpc, common_height)
                follower_block = block(args.follower_rpc, common_height)
                if sequencer_block["hash"] != follower_block["hash"]:
                    fail(f"nodes disagree at common height {common_height}")
                samples += 1
                log(
                    f"OK: sample {samples}/{args.samples}; block {common_height} hash matches",
                    "green",
                )
            sequencer_start = sequencer_latest

    if samples < args.samples:
        fail(f"only collected {samples}/{args.samples} block samples in {args.wait:g}s")
    log("OK: block production, follower catch-up, and block hashes are consistent", "green")


def main():
    # Keep orchestration here small: validate the enclave first, then validate
    # the actual chain and synchronization behavior over public RPC.
    global DIAGNOSTIC_ENCLAVE
    global USE_COLOR
    args = parse_args()
    DIAGNOSTIC_ENCLAVE = args.enclave
    USE_COLOR = not args.no_color and sys.stdout.isatty() and os.environ.get("CI") != "true"
    check_enclave(args.enclave)
    check_network(args)
    log("PASS: Arkiv Kurtosis network health check", "green")


if __name__ == "__main__":
    main()
