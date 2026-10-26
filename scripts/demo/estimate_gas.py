#!/usr/bin/env python3
"""A delete sent with eth_estimateGas's gas limit never gets mined.

Starts a dev node like run.py, but on genesis/dev.json without Osaka (as on the
kurtosis devnet), creates an entity and deletes it with arkiv-cli, which uses
eth_estimateGas's result as the gas limit. The estimate for the delete is the
whole block's gas limit, and every block starts with the protocol purge tx, so
the delete can never fit.
"""

import json
import os
import subprocess
import tempfile

from run import DEV_KEY, ROOT, binary, find_binary, free_port, log, rpc, wait_for, wait_for_next_block

port = free_port()


def cli(*args, timeout=None):
    out = subprocess.run(
        [str(find_binary("arkiv-cli", "ARKIV_CLI_BINARY")), "--rpc-url",
         f"http://127.0.0.1:{port}", "--private-key", DEV_KEY, *args],
        capture_output=True, text=True, check=True, timeout=timeout,
    ).stdout
    return dict(map(str.strip, l.split(": ", 1)) for l in out.splitlines() if ": " in l)


with tempfile.TemporaryDirectory() as scratch:
    genesis = json.loads((ROOT / "genesis/dev.json").read_text())
    del genesis["config"]["osakaTime"]
    with open(f"{scratch}/genesis.json", "w") as f:
        json.dump(genesis, f)
    log("starting a dev node without Osaka")
    node = subprocess.Popen(
        [str(binary()), "node", "--dev", "--dev.block-time", "1sec",
         "--chain", f"{scratch}/genesis.json", "--datadir", f"{scratch}/chain",
         "--http", "--http.api", "eth,txpool", "--http.port", str(port)],
        env={**os.environ, "ARKIV_STORE_PATH": f"{scratch}/golemdb"},
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        wait_for("rpc", lambda: rpc(port, "eth_blockNumber"), timeout=120)
        log("creating an entity")
        key = cli("create", "--payload", "x", "--min-lifetime", "1000")["entity_key"]
        wait_for_next_block(port)  # the store lags a block; deleting at once is EntityNotFound
        log("deleting it, gas limit from eth_estimateGas; waiting 10s for it to be mined")
        start = int(rpc(port, "eth_blockNumber"), 16)
        try:
            cli("delete", "--key", key, timeout=10)
            log("delete was mined")
        except subprocess.TimeoutExpired:
            (tx,) = (t for txs in rpc(port, "txpool_content")["pending"].values()
                     for t in txs.values())
            block = rpc(port, "eth_getBlockByNumber", ["latest", False])
            print(f"delete gas limit (the estimate): {int(tx['gas'], 16)}")
            print(f"block gas limit:                 {int(block['gasLimit'], 16)}")
            print(f"still pending after {int(block['number'], 16) - start} blocks")
    finally:
        node.terminate()
        node.wait()
