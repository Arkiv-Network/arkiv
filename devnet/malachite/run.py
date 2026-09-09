#!/usr/bin/env python3
"""Three equal-weight Malachite validators + three native arkiv-reth processes."""
import argparse
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import subprocess
import time
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
VALIDATOR_COUNT = 3


def rpc(node, method, params=()):
    request = urllib.request.Request(
        f"http://127.0.0.1:{18545 + node}",
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)}).encode(),
        {"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=3) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(result["error"])
    return result["result"]


def wait_for(description, predicate, timeout=90):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, ValueError, RuntimeError) as error:
            last = error
        time.sleep(0.25)
    raise RuntimeError(f"Timed out: {description}; last error: {last}")


def finalized(node):
    block = rpc(node, "eth_getBlockByNumber", ["finalized", False])
    return int(block["number"], 16) if block else 0


def agreement(nodes, height):
    if any(finalized(i) < height for i in nodes):
        return False
    blocks = [rpc(i, "eth_getBlockByNumber", [hex(height), False]) for i in nodes]
    assert len({b["hash"] for b in blocks}) == 1, "conflicting finalized hashes"
    assert len({b["stateRoot"] for b in blocks}) == 1, "conflicting state roots"
    return True


def build():
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--profile",
            "dev-fast",
            "-p",
            "arkiv-reth",
            "-p",
            "arkiv-cli",
        ],
        cwd=ROOT,
        check=True,
    )
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--manifest-path",
            str(HERE / "Cargo.toml"),
            "-p",
            "malachitebft-eth-app",
        ],
        cwd=ROOT,
        check=True,
    )


def binaries():
    return [
        Path(os.environ.get(name, default)).resolve()
        for name, default in [
            ("ARKIV_RETH_BINARY", ROOT / "target/dev-fast/arkiv-reth"),
            ("ARKIV_CLI_BINARY", ROOT / "target/dev-fast/arkiv-cli"),
            ("ARKIV_CONSENSUS_BINARY", HERE / "target/debug/malachitebft-eth-app"),
        ]
    ]


def initialize(data, cli, consensus):
    if (data / "genesis.json").exists():
        raise RuntimeError(f"{data} already has a devnet; use a new --data-dir for a fresh run")
    data.mkdir(parents=True, exist_ok=True)
    config = {
        key: 0
        for key in [
            "homesteadBlock",
            "eip150Block",
            "eip155Block",
            "eip158Block",
            "byzantiumBlock",
            "constantinopleBlock",
            "petersburgBlock",
            "istanbulBlock",
            "muirGlacierBlock",
            "berlinBlock",
            "londonBlock",
        ]
    }
    config.update(
        chainId=64331,
        terminalTotalDifficulty=0,
        terminalTotalDifficultyPassed=True,
        shanghaiTime=0,
        cancunTime=0,
    )
    genesis = {
        "config": config,
        "nonce": "0x0",
        "timestamp": hex(int(time.time())),
        "extraData": "0x",
        "gasLimit": "0x3938700",
        "difficulty": "0x0",
        "mixHash": "0x" + "00" * 32,
        "coinbase": "0x" + "00" * 20,
        "baseFeePerGas": "0x3b9aca00",
        "alloc": {},
    }
    (data / "genesis.json").write_text(json.dumps(genesis, indent=2))
    (data / "jwtsecret").write_text(secrets.token_hex(32))
    (data / "jwtsecret").chmod(0o600)
    subprocess.run([str(cli), "inject-predeploy", str(data / "genesis.json")], check=True)
    subprocess.run(
        [
            str(consensus),
            "testnet",
            "--nodes",
            str(VALIDATOR_COUNT),
            "--home",
            str(data / "consensus"),
            "--runtime",
            "multi-threaded:2",
        ],
        check=True,
    )


def smoke(cli, processes=None, restart=None):
    nodes = list(range(VALIDATOR_COUNT))
    wait_for("three nodes finalizing", lambda: agreement(nodes, 4))
    print("PASS: three nodes agree on finalized hash and state root", flush=True)
    # Submit to each node, exercising EL transaction gossip and rotating proposers.
    for node in nodes:
        output = subprocess.check_output(
            [
                str(cli),
                "--rpc-url",
                f"http://127.0.0.1:{18545 + node}",
                "create",
                "--payload",
                f"malachite-{node}",
                "--min-lifetime",
                "8",
            ],
            text=True,
            timeout=60,
        )
        key = re.search(r"entity_key:\s*(0x[0-9a-fA-F]+)", output).group(1)
        tx = re.search(r"tx:\s*(0x[0-9a-fA-F]+)", output).group(1)
        expiry = int(re.search(r"expires_at:\s*(\d+)", output).group(1))
        if node == 0:
            old_expiry = expiry
            extended = subprocess.check_output(
                [
                    str(cli),
                    "--rpc-url",
                    "http://127.0.0.1:18546",
                    "extend-expiry",
                    "--key",
                    key,
                    "--min-lifetime",
                    "16",
                ],
                text=True,
                timeout=60,
            )
            expiry = int(re.search(r"expires_at:\s*(\d+)", extended).group(1))
            wait_for("old expiry passed", lambda: agreement(nodes, old_expiry + 1))
            assert all(
                rpc(i, "arkiv_debugEntityExists", [key]) for i in nodes
            ), "extended entity purged at old expiry"
            print("PASS: expiry extension prevents premature pruning", flush=True)
        receipt = rpc(node, "eth_getTransactionReceipt", [tx])
        assert receipt["status"] == "0x1", receipt
        height = int(receipt["blockNumber"], 16)
        wait_for("create finalized everywhere", lambda: agreement(nodes, height))
        entities = [rpc(i, "arkiv_getEntity", [key, height]) for i in nodes]
        assert entities[0] is not None and all(e == entities[0] for e in entities), entities
        wait_for("expiry finalized", lambda: agreement(nodes, expiry + 2))
        # The debug method bypasses logical expiry filtering, proving physical deletion.
        wait_for(
            "entity physically purged on all nodes",
            lambda: all(rpc(i, "arkiv_debugEntityExists", [key]) is False for i in nodes),
        )
        print(f"PASS: write submitted to node {node}, finalized and purged everywhere", flush=True)
    if processes is not None:
        # Terminate consensus only; retain EL to check its finalized view remains unchanged.
        victim = processes[-1]
        victim.terminate()
        victim.wait(timeout=15)
        # Allow any in-flight quorum certificate to finish before observing the halt.
        time.sleep(3)
        before = [finalized(i) for i in [0, 1]]
        time.sleep(5)
        assert before == [finalized(i) for i in [0, 1]], "two validators finalized without quorum"
        print("PASS: two of three validators cannot advance finality", flush=True)
        height = max(finalized(i) for i in nodes) + 4
        restart(2)
        wait_for("all three validators resume", lambda: agreement(nodes, height))
        print(
            "PASS: restarting the third validator restores finality with matching state", flush=True
        )


def up(data, check):
    reth, cli, consensus = binaries()
    for binary in [reth, cli, consensus]:
        if not binary.is_file():
            raise RuntimeError(f"Missing {binary}; run run.py build first")
    # Fail before creating data or starting processes if any required port is occupied.
    for base in [18545, 18551, 30303, 27000, 28000, 29000]:
        for i in range(VALIDATOR_COUNT):
            with socket.socket() as sock:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                sock.bind(("127.0.0.1", base + i))
                sock.listen(1)
    initialize(data, cli, consensus)
    processes, logs = [], []

    def spawn(command, name, env=None):
        log = open(data / f"{name}.log", "a")
        logs.append(log)
        process = subprocess.Popen(
            [str(arg) for arg in command], stdout=log, stderr=subprocess.STDOUT, env=env, cwd=ROOT
        )
        processes.append(process)
        return process

    def interrupt(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)
    try:
        for i in range(VALIDATOR_COUNT):
            spawn(
                [
                    reth,
                    "node",
                    "--chain",
                    data / "genesis.json",
                    "--datadir",
                    data / f"el-{i}",
                    "--http",
                    "--http.addr",
                    "127.0.0.1",
                    "--http.port",
                    18545 + i,
                    "--http.api",
                    "eth,net,web3,admin,txpool",
                    "--authrpc.addr",
                    "127.0.0.1",
                    "--authrpc.port",
                    18551 + i,
                    "--authrpc.jwtsecret",
                    data / "jwtsecret",
                    "--port",
                    30303 + i,
                    "--disable-discovery",
                    "--addr",
                    "127.0.0.1",
                    "--nat",
                    "none",
                    "--ipcdisable",
                ],
                f"el-{i}",
            )
        wait_for(
            "execution RPCs",
            lambda: all(rpc(i, "eth_chainId") == hex(64331) for i in range(VALIDATOR_COUNT)),
        )
        enodes = [rpc(i, "admin_nodeInfo")["enode"] for i in range(VALIDATOR_COUNT)]
        for i in range(VALIDATOR_COUNT):
            for j in range(VALIDATOR_COUNT):
                if i != j:
                    rpc(i, "admin_addPeer", [enodes[j]])

        def start_consensus(i):
            env = dict(
                os.environ,
                ARKIV_ENGINE_URL=f"http://127.0.0.1:{18551 + i}",
                ARKIV_ETH_URL=f"http://127.0.0.1:{18545 + i}",
                ARKIV_JWT_PATH=str(data / "jwtsecret"),
            )
            spawn(
                [consensus, "start", "--home", data / "consensus" / str(i), "--log-level", "info"],
                f"cl-{i}",
                env,
            )

        for i in range(VALIDATOR_COUNT):
            start_consensus(i)

        def restart_consensus(i):
            start_consensus(i)
            processes[VALIDATOR_COUNT + i] = processes.pop()

        wait_for("initial finality", lambda: agreement(range(VALIDATOR_COUNT), 2))
        print(
            f"Three-validator devnet running. RPC: 127.0.0.1:18545–18547; logs: {data}", flush=True
        )
        if check:
            smoke(cli, processes, restart_consensus)
        else:
            while True:
                for process in processes:
                    if process.poll() is not None:
                        raise RuntimeError(f"Node exited ({process.returncode}); inspect {data}")
                time.sleep(1)
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
        for process in processes:
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for log in logs:
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["build", "up", "check"])
    parser.add_argument("--data-dir", type=Path, default=ROOT / "data/malachite")
    parser.add_argument(
        "--check", action="store_true", help="run writes, pruning and quorum smoke tests, then stop"
    )
    args = parser.parse_args()
    if args.command == "build":
        build()
    elif args.command == "check":
        smoke(binaries()[1])
    else:
        up(args.data_dir.resolve(), args.check)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
