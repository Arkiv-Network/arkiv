#!/usr/bin/env python3
"""Run arkiv-reth against a throwaway Valkey for a few seconds, then tidy up.

Everything this starts is its own: a Valkey server on a free port with its own
data directory, and a dev chain with its own datadir. Nothing touches a Valkey
you already run, and the namespace is unique per run, so two of these can
overlap without colliding.

The point is the evidence, not the uptime. Every run leaves one directory
holding both logs, the chain's progress, and what actually landed in the store
-- the keys, and one account record decoded far enough to show that Arkiv's
cells carry the `$` prefix the GolemDB spec reserves for a host.

Processes and the scratch data are removed on the way out, including after a
failure or a Ctrl-C. The log directory is what survives.

Usage: scripts/demo/run.py [seconds] [--keep] [--out DIR]

  seconds     how long to let the chain run (default 30)
  --keep      leave Valkey and the chain running, and say how to reach them
  --out DIR   where to write the bundle (default target/demo/<timestamp>)
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# reth --dev's first prefunded account, and the published test key for it that
# anvil and ethereum-package both ship. Not a secret, and worthless outside a
# throwaway dev chain -- it is here so the run can spend something.
DEV_ACCOUNT = "f39fd6e51aad88f6f4ce6ab8827279cfffb92266"
DEV_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"


def log(message):
    """Print a flushed, timestamped step marker for local and CI runs."""
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    print(f"[{timestamp}] {message}", flush=True)


def free_port():
    """A port nothing is listening on, released right before we hand it over.

    Racy in principle. In practice the kernel does not hand the same ephemeral
    port out twice in the microseconds before the server binds it, and the
    alternative -- a fixed port -- collides with the Valkey most developers
    already have running on 6379.
    """
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def wait_for(label, probe, timeout):
    """Poll `probe` until it returns something truthy, or give up loudly."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        time.sleep(0.25)
    raise SystemExit(f"timed out after {timeout}s waiting for {label}")


def rpc(port, method, params=None):
    """One JSON-RPC call, or None while the node is still coming up."""
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}
    ).encode()
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}",
        data=body,
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            return json.load(response).get("result")
    except (urllib.error.URLError, TimeoutError, ConnectionError, json.JSONDecodeError):
        return None


def valkey(port, *args):
    """Run one valkey-cli command against our own server."""
    out = subprocess.run(
        ["valkey-cli", "-p", str(port), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    return out.stdout.strip()


def binary():
    """The arkiv-reth to run: ARKIV_RETH_BINARY, else whichever profile built."""
    override = os.environ.get("ARKIV_RETH_BINARY")
    if override:
        path = Path(override)
        if not path.exists():
            raise SystemExit(f"ARKIV_RETH_BINARY is set but {path} does not exist")
        return path
    for profile in ("dev-fast", "debug", "release"):
        path = ROOT / "target" / profile / "arkiv-reth"
        if path.exists():
            return path
    raise SystemExit(
        "no arkiv-reth binary found -- build one first:\n"
        "  cargo build --bin arkiv-reth --profile dev-fast\n"
        "or point ARKIV_RETH_BINARY at one."
    )


def spend_something(rpc_port, out):
    """Send one transfer, so the chain writes an account record.

    An idle dev chain never debits anybody, and the account record is where
    the interesting cell names live -- without a transaction the store holds
    only commit bookkeeping. Needs `cast`; if foundry is not installed the run
    still works and the summary says the record is missing and why.
    """
    if not shutil.which("cast"):
        return "skipped: cast not on PATH, so nothing was spent"
    out_file = out / "transfer.log"
    result = subprocess.run(
        [
            "cast",
            "send",
            "--rpc-url",
            f"http://127.0.0.1:{rpc_port}",
            "--private-key",
            DEV_KEY,
            # Somewhere to send it. The recipient does not matter; the point
            # is that the sender is debited.
            "0x000000000000000000000000000000000000dEaD",
            "--value",
            "1ether",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    out_file.write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        return f"failed (see transfer.log): {result.stderr.strip().splitlines()[-1:]}"
    return "sent 1 ETH from the dev account"


def dump_store(port, namespace, out, spend):
    """Write what the run left in Valkey: the keys, and one account record.

    The account record is the interesting one. Its cell names are what carries
    the host/engine prefix split, and reading them back off the wire is the
    only check that does not trust our own code to report what it wrote.
    """
    keys = sorted(valkey(port, "--scan", "--pattern", f"{namespace}:*").splitlines())
    (out / "valkey-keys.txt").write_text("\n".join(keys) + "\n")

    kinds = {}
    for key in keys:
        parts = key.split(":")
        kinds[parts[1] if len(parts) > 1 else key] = (
            kinds.get(parts[1] if len(parts) > 1 else key, 0) + 1
        )

    lines = [f"total keys: {len(keys)}", f"transfer: {spend}", ""]
    lines += ["by kind:"] + [f"  {n:>4}  {k}" for k, n in sorted(kinds.items())]

    records = [key for key in keys if ":rec:" in key]
    lines.append("")
    if records:
        # HKEYS, not HGETALL: the values are raw bytes and would garble the
        # file. The names carry the prefix split, which is what to look at.
        sample = records[0]
        fields = sorted(valkey(port, "hkeys", sample).splitlines())
        lines += [
            f"{len(records)} record(s); cells of {sample}:",
            "  " + ", ".join(fields),
            "",
            "  $-prefixed cells are Arkiv's, under the one prefix GolemDB's",
            "  grammar leaves to a host protocol. #-prefixed cells are the",
            "  engine's own.",
        ]
    else:
        lines += [
            "no entity records: this run only moved money, and Ethereum",
            "balances and nonces live on reth, not in the Arkiv store (see",
            "HostStateView). The store holds entity records and the commit",
            "lineage; records appear once something creates an entity.",
        ]
    (out / "store-summary.txt").write_text("\n".join(lines) + "\n")
    return keys


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__)
    parser.add_argument("seconds", nargs="?", type=int, default=30)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--out", type=Path, default=None)
    args = parser.parse_args()

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = (args.out or ROOT / "target/demo" / stamp).resolve()
    out.mkdir(parents=True, exist_ok=True)

    node_bin = binary()
    valkey_port, rpc_port = free_port(), free_port()
    namespace = f"demo{stamp}"
    scratch = out / "scratch"
    (scratch / "valkey").mkdir(parents=True, exist_ok=True)

    processes = []

    def cleanup():
        for name, proc in processes:
            if proc.poll() is None:
                log(f"stopping {name}")
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
        shutil.rmtree(scratch, ignore_errors=True)

    # Ctrl-C and SIGTERM have to reach cleanup too, or a cancelled run leaves a
    # Valkey and a chain behind.
    def on_signal(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}")

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)

    try:
        log(f"logs will be in {out}")

        valkey_log = (out / "valkey.log").open("w")
        processes.append(
            (
                "valkey",
                subprocess.Popen(
                    [
                        "valkey-server",
                        "--port",
                        str(valkey_port),
                        "--dir",
                        str(scratch / "valkey"),
                        # No snapshotting: this data is scrap and a stray
                        # dump.rdb in the repo would be worse than useless.
                        "--save",
                        "",
                        "--appendonly",
                        "no",
                    ],
                    stdout=valkey_log,
                    stderr=subprocess.STDOUT,
                ),
            )
        )
        wait_for(
            f"valkey on {valkey_port}",
            lambda: valkey(valkey_port, "ping") == "PONG",
            timeout=20,
        )
        log(f"valkey up on {valkey_port}, namespace {namespace}")

        node_log = (out / "node.log").open("w")
        processes.append(
            (
                "arkiv-reth",
                subprocess.Popen(
                    [
                        str(node_bin),
                        "node",
                        "--dev",
                        "--dev.block-time",
                        "1sec",
                        "--datadir",
                        str(scratch / "chain"),
                        "--http",
                        # `arkiv` is not a selectable module: the arkiv_*
                        # namespace is merged by extend_rpc_modules and rides
                        # on whichever transports are already on.
                        "--http.api",
                        "eth,net,web3",
                        "--http.port",
                        str(rpc_port),
                    ],
                    env={
                        **os.environ,
                        "ARKIV_STORE_URL": f"redis://127.0.0.1:{valkey_port}",
                        "ARKIV_STORE_NAMESPACE": namespace,
                    },
                    stdout=node_log,
                    stderr=subprocess.STDOUT,
                ),
            )
        )
        wait_for(
            f"rpc on {rpc_port}",
            lambda: rpc(rpc_port, "eth_blockNumber") is not None,
            timeout=120,
        )
        log(f"node up on {rpc_port}, running for {args.seconds}s")

        spend = spend_something(rpc_port, out)
        log(f"transfer: {spend}")

        # Sample while it runs, so the bundle shows the chain moving rather
        # than just a first and last number.
        samples = []
        started = time.monotonic()
        while time.monotonic() - started < args.seconds:
            block = rpc(rpc_port, "eth_blockNumber")
            samples.append(
                {
                    "elapsed": round(time.monotonic() - started, 1),
                    "block": int(block, 16) if block else None,
                    "entities": rpc(rpc_port, "arkiv_getEntityCount"),
                    "keys": len(
                        valkey(valkey_port, "--scan", "--pattern", f"{namespace}:*").splitlines()
                    ),
                }
            )
            time.sleep(max(1, args.seconds // 10))
        (out / "progress.json").write_text(json.dumps(samples, indent=2) + "\n")

        keys = dump_store(valkey_port, namespace, out, spend)
        first, last = samples[0]["block"], samples[-1]["block"]
        log(f"blocks {first} -> {last}, {len(keys)} keys in the store")

        if first is None or last is None or last <= first:
            raise SystemExit(f"the chain did not advance: {first} -> {last}")
        if not keys:
            raise SystemExit("the chain advanced but the store is empty")

        if args.keep:
            log(f"--keep: valkey on {valkey_port}, rpc on {rpc_port}, namespace {namespace}")
            log("stop them yourself; scratch data is in " + str(scratch))
            processes.clear()
            return 0

        log("ok")
        return 0
    except KeyboardInterrupt:
        log("interrupted")
        return 130
    finally:
        if not args.keep:
            cleanup()
        log(f"logs in {out}")


if __name__ == "__main__":
    sys.exit(main())
