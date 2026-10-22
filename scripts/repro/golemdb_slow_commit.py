#!/usr/bin/env python3
"""Reproduce the chain halt a slow GolemDB commit causes.

Starts a dev node on a throwaway GolemDB with block N's commit held up (the
node's ARKIV_DEBUG_SLOW_COMMIT_HEIGHT hook), then pings it every half second
and prints what it sees. Blocks built while the commit runs fail to save, and
the chain halts about 30 blocks later. Ctrl-C to stop.

Usage: scripts/repro/golemdb_slow_commit.py [height] [delay_ms]   (default 20 2000)
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BINARY = os.environ.get("ARKIV_RETH_BINARY", ROOT / "target/release-fast/arkiv-reth")
PORT = 18545
# Log lines worth echoing as they appear.
INTERESTING = re.compile(r"slow commit for repro|failed to adopt|invalid forkchoice state")


def rpc(method, *params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)})
    request = urllib.request.Request(
        f"http://127.0.0.1:{PORT}", body.encode(), {"content-type": "application/json"}
    )
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            return json.load(response).get("result")
    except OSError:
        return None


height = sys.argv[1] if len(sys.argv) > 1 else "20"
delay_ms = sys.argv[2] if len(sys.argv) > 2 else "2000"
# tmpfs, so the disk plays no part: the only slow commit is the one we ask for.
data = Path(tempfile.mkdtemp(prefix="arkiv-repro-", dir="/dev/shm"))
log_path = data / "node.log"
print(f"slowing block {height}'s commit by {delay_ms} ms; node log: {log_path}")

node = subprocess.Popen(
    [
        str(BINARY), "node", "--dev", "--dev.block-time", "250ms",
        "--datadir", str(data / "reth"), "--log.file.directory", str(data / "logs"),
        "--db.growth-step", "64MB",
        "--http", "--http.port", str(PORT), "--http.api", "eth",
        "--authrpc.port", "18551", "--port", "13030", "--disable-discovery", "--ipcdisable",
    ],
    env={
        **os.environ,
        "ARKIV_STORE_URL": f"golemdb://{data / 'golemdb'}",
        "ARKIV_DEBUG_SLOW_COMMIT_HEIGHT": height,
        "ARKIV_DEBUG_SLOW_COMMIT_MS": delay_ms,
    },
    stdout=log_path.open("w"),
    stderr=subprocess.STDOUT,
)  # fmt: skip

try:
    log = log_path.open()
    last_head, since, refused = None, time.monotonic(), False
    while node.poll() is None:
        time.sleep(0.5)
        for line in log.readlines():
            line = re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
            # The refused forkchoice update repeats every block time; once is enough.
            if INTERESTING.search(line) and not (refused and "forkchoice" in line):
                refused = refused or "Error advancing the chain" in line
                print("  log:", line[:150])
        head = rpc("eth_blockNumber")
        if head is None:
            print("node not answering yet")
            continue
        if head != last_head:
            last_head, since = head, time.monotonic()
            print(f"head {int(head, 16)}")
        else:
            print(f"head {int(head, 16)}  STALLED for {time.monotonic() - since:.0f}s")
except KeyboardInterrupt:
    pass
finally:
    node.terminate()
    node.wait()
    shutil.rmtree(data, ignore_errors=True)
