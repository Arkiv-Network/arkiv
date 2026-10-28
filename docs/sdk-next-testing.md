# Testing the tokenomics node with the Arkiv SDK

Status on 2026-10-28: picked up again on `matthiaszimmermann/test/sdk-next`, branched from the
tokenomics branch (#230). This note is the context and the step-by-step guide.

## Goal

Drive a node built from the tokenomics branch through the Arkiv SDK, and check the
tokenomics from outside: the SDK's gas estimates are accepted, `gasUsed` includes storage
gas, the pool at `ARKIV_ADDRESS` grows, and reads return what was written. This is also the
plan's step 8, "Integration check".

## Findings so far

- **The SDK to use is `@arkiv-network/sdk@next`**, recommended by the SDK owner. On
  2026-10-28 `next` moved from `0.9.0-next.2` (published 2026-10-26) to `0.9.0-next.3`
  (published that evening); other tags were `latest` 0.8.1, `dev` 0.8.1-dev.1 and `advanced`
  0.8.0-advanced.3. Pin the exact version: the tag moves. `kurtosis/integration` pins next.3.
- **It speaks this node's protocol.** Checked by searching the package, not by running it:
  it sends `execute(...)` to `0x4400…0044`, uses `entityNonce`, `arkiv_query`,
  `arkiv_getEntity` and `arkiv_getBlockTiming`, and ships a `localhost` chain with id 1337,
  the dev chain's id. It exports `createWalletClient`, `createPublicClient`, `createEntity`,
  `extendEntity`, `deleteEntity` and `changeOwnership`. Read its `.d.ts` files for the exact
  signatures before writing tests.
- **SDK 0.7, which `kurtosis/integration` uses today, does not**: it sends compressed RLP to
  a different address, so that project tests no entity writes yet (see its README). Its tests
  use the SDK only for `arkiv_getBlockTiming`, `arkiv_getEntityCount` and plain Ethereum calls.
- **No new repository is needed.** `kurtosis/integration` is a bun project with viem, and
  `ARKIV_RPC_URL` / `ARKIV_FUNDED_KEY` let it target a plain dev node.
- **bun is in the dev container** (`.devcontainer/Dockerfile`, latest release, as CI's
  `setup-bun`). The commit is on this branch for now; it goes to `main` in its own PR.
- **CI already runs the Kurtosis network on every PR**, whatever its base:
  `pr-artifact.yml` triggers on `pull_request` with no branch filter and runs `kurtosis.yml`
  on the PR's own binary. A PR against the tokenomics branch is tested on the tokenomics node.
- **State is served at the chain head only** on GolemDB (#145): `eth_getBalance` and the like
  at a past block fail. Read balances at `latest`, around the transaction.
- **Kurtosis needs Docker, the dev container has none.** Develop against a local
  `arkiv-reth node --dev` (below); the two-node network runs in CI. Kurtosis on the host is
  the fallback for a failure only CI shows.

## How Kurtosis builds its chain

For reference; nothing here needs changing for the SDK tests.

- **Genesis:** generated per enclave by `kurtosis/genesis-generator/entrypoint.sh`. The stock
  ethereum-package generator makes `genesis.json` (chain id, prefunded accounts from the
  package's mnemonic). The script replaces its `config.arkiv` with the one from
  `arkiv-reth dump-genesis --chain dev`, so Kurtosis always runs the **dev chain's fee
  schedule**. It then builds block 0 with `arkiv-reth init` and hands it to the CL genesis as
  its shadow-fork block.
- **Node:** not `--dev`. ethereum-package's reth launcher runs
  `arkiv-reth node --datadir=/data/reth/execution-data --chain=/network-configs/genesis.json
  --http … --authrpc.jwtsecret=/jwt/jwtsecret …`, Lighthouse proposes a block every 2 s
  through the Engine API. `kurtosis/arkiv-chain.yaml` adds
  `ARKIV_STORE_PATH=/data/reth/execution-data/golemdb`, and
  `--engine.backfill-threshold=33` for the follower.
- **Images:** `docker/arkiv-reth.Dockerfile` (from source) or
  `docker/arkiv-reth-prebuilt.Dockerfile` (CI, prebuilt binary), plus
  `kurtosis/genesis-generator/Dockerfile`; all built by `scripts/kurtosis/up.py`.

To try a different fee schedule on Kurtosis, `entrypoint.sh` would have to take
`config.arkiv` from elsewhere. Not needed now: experiments run on the local node.

## A local dev node with your own genesis

`arkiv-reth node --dev` without `--chain` runs on `genesis/dev.json`, built into the binary:
chain id 1337, the 20 hardhat accounts with 1,000,000 ETH each, gas limit 30M, and
`config.arkiv` with the GolemDB limits and the fee schedule (`capacity_rate` 14,
`cell_byte_units` 1, `index_byte_units` 1, `c_cell` 64, `c_idx` 64, `max_lifetime`
7,884,000). To experiment with the schedule, give `--dev` a genesis file of your own.

Each step below has a check. Run the node in its own terminal (step 3); the other steps go
in a second one, from the repository root.

### 1. Build

```sh
cargo build -j 1 -p arkiv-reth -p arkiv-cli
```

`-j 1`: with rust-analyzer running, linking `arkiv-reth` with more jobs runs out of memory
in the container.

**Check:** `./target/debug/arkiv-reth --version` and `./target/debug/arkiv-cli --version`
print a version (`Reth Version: …` for the node).

### 2. Make a genesis file

Copy `genesis/dev.json` and change only `config.arkiv.tokenomics`. That keeps chain id 1337
and the funded accounts, so `arkiv-cli` and the SDK tests' `ARKIV_FUNDED_KEY` work unchanged:

```sh
mkdir -p /tmp/arkiv-genesis
jq '.config.arkiv.tokenomics.capacity_rate = 28' genesis/dev.json > /tmp/arkiv-genesis/rate28.json
```

Do not edit `genesis/dev.json` itself: it is the built-in dev chain, the chain-spec tests
read it, and Kurtosis copies its `config.arkiv`.

The node only requires every schedule value to be positive (it refuses 0 at startup and
names the field). It does not check that entities still fit a transaction: with a high
enough rate an ordinary create needs more than the per-transaction cap of 2^24 gas, and the
estimate fails. That is a result, not a bug.

**Check:**

```sh
jq -c '{chainId: .config.chainId, alloc: (.alloc|length), tokenomics: .config.arkiv.tokenomics}' \
  /tmp/arkiv-genesis/rate28.json
```

shows `"chainId":1337,"alloc":20` and your values. Another chain id means the file is not a
copy of `genesis/dev.json`: `genesis/example.json`, for one, has chain id 424242 and funds
only `0x1111…` and `0x2222…`, whose keys nobody has.

### 3. Start the node

```sh
G=/tmp/arkiv-genesis/rate28.json
D=/tmp/arkiv-dev-$(basename $G .json)
rm -rf $D
ARKIV_STORE_PATH=$D/golemdb ./target/debug/arkiv-reth node \
  --dev --dev.block-time 2s --chain $G \
  --datadir $D \
  --http --http.api eth,net,web3,txpool \
  --disable-discovery --ipcdisable
```

- `ARKIV_STORE_PATH`: GolemDB, as on Kurtosis. The node refuses to start without a store.
- `--dev.block-time 2s`: Kurtosis's slot time. Without it `--dev` seals only when a
  transaction arrives, and the SDK, which turns lifetimes into blocks from the block timing,
  sees irregular blocks.
- `--http.api`: `arkiv` is not listed because reth refuses unknown names; the `arkiv_*`
  methods are served anyway (see `docker/compose.yml`).
- `--disable-discovery --ipcdisable`: no p2p discovery and no IPC socket, as the e2e
  harness starts its nodes.
- A fresh datadir per genesis file: the fee schedule is part of block 0, so another schedule
  gives another genesis hash, and an old datadir does not match it.

**Check:** after a few seconds the log shows `Block added to canonical chain number=…`, one
block every 2 s.

### 4. Check that the node answers

In the second terminal:

```sh
rpc() { curl -s -X POST -H 'Content-Type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" \
  http://127.0.0.1:8545 | jq -r 'if .error then "error: \(.error.message)" else .result end'; }

rpc eth_chainId '[]'
rpc eth_getBalance '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]'
./target/debug/arkiv-cli block-timing
```

**Check:** `0x539` (1337); `0xd3c21bcecceda1000000` (1,000,000 ETH); a block number that
grows by one every 2 s, with `duration: 2s`.

If the chain id is another one or the balance is `0x0`, stop here: the node runs on a
genesis that does not fund `arkiv-cli`'s default key, and step 5 fails with
`out of gas: gas required exceeds: 0` (the estimate caps gas at what the sender can pay,
which is nothing). Stop the node and go back to step 2.

### 5. Check that the schedule is used

Read the pool's balance, create an entity, read it again. The pool at `ARKIV_ADDRESS`
(`0x4400…0044`) receives `base_fee × storage_gas`, so its growth divided by the base fee of
the transaction's block is the transaction's storage gas:

```sh
POOL=0x4400000000000000000000000000000000000044
BEFORE=$(rpc eth_getBalance "[\"$POOL\",\"latest\"]")
./target/debug/arkiv-cli create --payload hello-tokenomics --min-lifetime 10000 \
  --attributes "kind:string=demo"
AFTER=$(rpc eth_getBalance "[\"$POOL\",\"latest\"]")
TX=0x…   # from the `tx:` line

RECEIPT=$(rpc eth_getTransactionReceipt "[\"$TX\"]")
BN=$(jq -r .blockNumber <<< "$RECEIPT")
GAS=$(( $(jq -r .gasUsed <<< "$RECEIPT") ))
BASE=$(( $(rpc eth_getBlockByNumber "[\"$BN\",false]" | jq -r .baseFeePerGas) ))
STORAGE=$(( (AFTER - BEFORE) / BASE ))
echo "status=$(jq -r .status <<< "$RECEIPT") gasUsed=$GAS storage=$STORAGE other=$(( GAS - STORAGE ))"
```

- **Both balances at `latest`, not at blocks `BN − 1` and `BN`.** On GolemDB the node serves
  state at the chain head only (#145); a read at a past block fails with
  `arkiv serves state at the chain head only: asked for commit …`. Receipts and block
  headers are reth's own, so `RECEIPT` and `BASE` can be read later.
- **Nothing else pays into the pool** on a private dev node, so its growth between the two
  reads is this transaction's. `arkiv-cli` returns once the receipt is there, so `AFTER`
  already includes it.
- **Check `BEFORE` and `AFTER` before the arithmetic:** bash counts a non-number as 0, so a
  failed read (`error: …`) would quietly make the result wrong.

**Check:** `status=0x1` and a positive `storage`. Measured on 2026-10-28 with this create:

| `capacity_rate` | gasUsed | storage | other |
| --- | --- | --- | --- |
| 14 (`genesis/dev.json`) | 112,013 | 197 | 111,816 |
| 28 | 112,209 | 393 | 111,816 |

Doubling the rate doubles the storage gas, rounded up; the rest does not move. A larger
payload or a longer `--min-lifetime` raises the storage part.

### 6. Stop the node

Ctrl-C in its terminal. The next genesis file starts again at step 2, with its own datadir.

## The tokenomics script

`kurtosis/integration/experiment/tokenomics.ts` creates entities through SDK-next and prints
what each one cost. It asserts nothing about prices, so it runs against any fee schedule; it
is not a test, and `bun test` does not pick it up (no `.test.ts` in its name).

Three kinds of entity, through unnamed schemas (a named one would add an indexed
`arkiv.schema` attribute to each entity):

| Kind | Attributes | Indexed | `body` |
| --- | --- | --- | --- |
| minimal | 1 | 0 | none |
| normal | 10 | 4 | 300 bytes |
| big | 20 | 15 | 100 KiB |

Each in three lifetimes: short (100 blocks), medium (a week, 302,400 blocks at the SDK's fixed
2 s per block) and long (`max_lifetime` of the schedule). That makes 9 combinations, each
created `COUNT` times, one transaction after the other.

### Run it

With a node from steps 1 to 4 running, from `kurtosis/integration`:

```sh
bun install --frozen-lockfile   # once
GENESIS=/tmp/arkiv-genesis/rate28.json bun ./kurtosis/integration/experiment/tokenomics.ts
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `GENESIS` | `genesis/dev.json` | The genesis file the node runs on. The node does not serve its fee schedule, so the long lifetime, `max_lifetime`, is read from here. Must be the node's file. |
| `COUNT` | 3 | Creates per combination |
| `ONLY` | all 9 | Some combinations only, e.g. `ONLY=big/long,normal/short` |
| `ARKIV_RPC_URL` | `http://127.0.0.1:8545` | The node |
| `ARKIV_FUNDED_KEY` | the first hardhat key | The sender; must be funded by the genesis |

It measures the storage gas as step 5 does: the pool's balance at `latest` before and after
each create, divided by the base fee of its block. So run it against a private node, where
nothing else pays into the pool, and not in parallel with other writes.

### What it prints

- The node, chain id, sender and the schedule read from `GENESIS`.
- One row per create: kind, lifetime in blocks, cells, indexed cells, `body` bytes, the
  SDK's gas limit, `gasUsed`, storage gas, the rest (`gasUsed − storage`), and the entity key.
  A failed create prints its reason and whether it was mined (and its gas paid) or not.
- **The overview:** per combination, the averages of `gasUsed`, storage gas, the rest, and the
  transaction fee in GLM (`gasUsed × effective gas price`), with how many creates succeeded.
- A read-back of one entity per kind through `getEntity`, compared with what was written. The
  script exits with 1 if any value differs.

**Check:** every combination `3/3` and every kind `matches what was written`. Measured on
2026-10-28 with `genesis/dev.json` (`capacity_rate` 14), averages of 3 creates:

| Kind | Lifetime | gasUsed | storage | other | tx fee (GLM) |
| --- | --- | --- | --- | --- | --- |
| minimal | short | 104,350 | 2 | 104,348 | 0.000104350 |
| minimal | medium | 109,182 | 4,806 | 104,376 | 0.000109182 |
| minimal | long | 229,653 | 125,277 | 104,376 | 0.000229653 |
| normal | short | 147,494 | 4 | 147,490 | 0.000147494 |
| normal | medium | 159,065 | 11,546 | 147,518 | 0.000159065 |
| normal | long | 448,559 | 301,032 | 147,526 | 0.000448559 |
| big | short | 4,162,940 | 150 | 4,162,790 | 0.004162940 |
| big | medium | 4,614,363 | 450,913 | 4,163,450 | 0.004614363 |
| big | long | 15,918,998 | 11,755,928 | 4,163,070 | 0.015918998 |

- **The rest depends on the kind, not the lifetime:** about 104k, 147k and 4.16M gas. For the
  big kind it is mostly the 100 KiB of calldata.
- **The fee** is `gasUsed` gwei here, since the dev chain's base fee stays at 1 gwei.
- **big/long only just fits:** the SDK's gas limit is 16.05M against the per-transaction cap
  of 2^24 = 16,777,216.
- **At `capacity_rate` 28** the storage gas doubles and the rest stays: normal/long uses
  750,169 gas. big/long no longer fits; the create fails with `out of gas` during the
  estimate and is not mined. The SDK reports it only as
  `the engine rejected the batch with Error`, without the reason; the script adds it.

## Plan for the SDK tests

1. **Branch** `matthiaszimmermann/test/sdk-next`, from the tokenomics branch (done). Open
   its PR against the tokenomics branch, and retarget it to `main` once #230 merges. Rebase it
   when #230 is rebased. Fix node bugs the tests find on the tokenomics branch, or in their
   own PR against `main`, not on this branch.
2. **Add the SDK beside 0.7**, under an alias, so the existing tests are unaffected:

   ```sh
   cd kurtosis/integration
   bun install --frozen-lockfile
   bun add @arkiv-network/sdk-next@npm:@arkiv-network/sdk@0.9.0-next.3
   ```

   Done. Once 0.9 is released, replace 0.7 with it and drop the alias.
3. **A client helper `src/devnet-next.ts`**: what `src/devnet.ts` does (chain id from the
   RPC, funded wallet), with SDK-next clients. `devnet.ts` stays as it is for the 0.7 tests.
4. **Write `test/sdk-writes.test.ts`**: one entity through create, patch, extend, transfer
   and delete, each with the SDK's own gas estimate. Per step:
   - the transaction is mined and succeeds;
   - between reads at `latest` just before and after it (GolemDB serves no past state), the
     pool at `ARKIV_ADDRESS` grows by a whole multiple of the base fee of the transaction's
     block, and by no more than `baseFee × gasUsed`. Loose on purpose: the exact numbers are pinned
     by the Rust e2e tests, and the check holds for any schedule, so it runs unchanged
     against every genesis file from the section above;
   - `getEntity` and a query on an attribute return what was written; after the delete the
     entity is gone.

   **Open:** gate it behind `ARKIV_SDK_WRITES=1` or not. Proposed: no gate, since CI's
   Kurtosis job runs this PR's node, which speaks the SDK's protocol.
5. **Run it** against the local node (steps 1 to 4 above, with `genesis/dev.json` or a file
   of your own), from `kurtosis/integration`:

   ```sh
   ARKIV_RPC_URL=http://127.0.0.1:8545 \
   ARKIV_FUNDED_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
   bun test
   bun run typecheck
   ```

6. **Update `kurtosis/integration/README.md`**: the new test, the alias, the local node.
7. **Later:** the SDK's own network-health suite against the same node, if its repository has
   one (the plan's step 8).
