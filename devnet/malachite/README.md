# Malachite + Arkiv local proof of concept

Four equal-weight validators, each paired with its own `arkiv-reth` execution
client. Three votes are required to finalize a block. This is an independent
local chain (ID 64331), with no Lighthouse or settlement chain.

Finality requires strictly more than two-thirds of voting power. With four
equal-weight validators, the chain can continue with one validator offline.

## Run

Requires the repository Rust toolchain, `protoc`, and Python 3. Docker is not
required. Run from the repository root:

```sh
python3 devnet/malachite/run.py build
python3 devnet/malachite/run.py up --check
```

`--check` checks four-node finality, writes submitted through every execution
endpoint, expiry extension and physical entity pruning, progress with one node stopped,
recovery after restart, and loss of progress with two validators stopped. All processes stop afterward;
data and logs remain in `data/malachite` for inspection.

For an interactive devnet, omit `--check`. Ctrl-C stops all eight processes.
In a second terminal, `python3 devnet/malachite/run.py check` runs the write and
pruning checks without stopping validators.

To test a random node failure on a fresh devnet:

```sh
python3 devnet/malachite/run.py recovery-check --data-dir data/malachite-recovery-next
```

This command kills both processes for one random node, checks that the other
three finalize at least four more blocks, then restarts the node with its saved
data. It checks that all four nodes reach the same finalized block and state.
All processes stop after the test. This test also runs as part of `up --check`.

Each launch requires a fresh data directory. To keep results from earlier runs:

```sh
python3 devnet/malachite/run.py up --data-dir data/malachite-next
```

Prebuilt binaries can be selected with `ARKIV_RETH_BINARY`, `ARKIV_CLI_BINARY`,
and `ARKIV_CONSENSUS_BINARY` (absolute paths). The launcher never deletes an
existing chain or stops processes it did not start.

| Interface | Node 0 | Node 1 | Node 2 | Node 3 |
| --- | --- | --- | --- | --- |
| Ethereum/Arkiv RPC | 18545 | 18546 | 18547 | 18548 |
| Authenticated Engine API | 18551 | 18552 | 18553 | 18554 |
| Execution P2P | 30303 | 30304 | 30305 | 30306 |
| Consensus P2P | 27000 | 27001 | 27002 | 27003 |
| Consensus metrics | 29000 | 29001 | 29002 | 29003 |

RPC, Engine API, and consensus connections use localhost. These are development
keys and funded test accounts, not production credentials.

## Design

This isolated Cargo workspace adapts the pinned Malaketh-layered example; see
[UPSTREAM.md](UPSTREAM.md) and the retained Apache license. It uses the published
`informalsystems-malachitebft-*` crates at version **0.5.0**, pinned exactly in
Cargo.toml and Cargo.lock. This is the latest release in the `informalsystems`
namespace; later Malachite releases use a different package namespace. The
older Alloy dependencies remain isolated from Arkiv's main workspace. The PoC intentionally
uses Cancun / Engine API V3, with later execution hardforks inactive.

- The proposer calls `forkchoiceUpdated` against the last decided parent, waits
  briefly for the Arkiv payload builder, and retrieves the complete payload.
- Votes identify the complete SSZ payload by Keccak-256. Every validator calls
  `newPayload` and requires `VALID` before approving a proposal.
- `newPayload` does not make proposals canonical. Only a Malachite decision
  causes `head = safe = finalized` to advance together.
- The payload and commit certificate are persisted together before fork choice
  advances. Sync transports the actual payload and certificate; historical
  decisions are retained. Startup checks local execution/consensus agreement
  and can replay a durable decision whose fork-choice update was interrupted.
- Genesis contains four independently generated validator keys with weight 1.
  Membership remains fixed; proposer selection rotates by height and round.
- Block timestamps use whole seconds and never intentionally run ahead of wall
  time. This PoC targets approximately one block per second, not subsecond block
  intervals. Finality is per block; round changes can take longer.

## Pruning

Each EL maintains its own `arkiv-pruning.db`. The proposer puts selected keys
inside the ordinary Arkiv protocol purge transaction; other validators execute
that exact transaction instead of selecting their own keys. Local pruning replay
lag can defer cleanup without changing validation.

The current pruning watermark records height rather than block hash. Keeping
undecided proposals noncanonical avoids poisoning this index across failed
rounds. This adapter must not be changed to advance speculative fork choice
without also making the pruning index fork-aware.

Entity lifetimes are block counts. The smoke test checks
`arkiv_debugEntityExists`, which bypasses logical expiry filtering, to prove
physical deletion rather than merely an expired entity disappearing from queries.

## Limits

This is an experimental local devnet, not a production consensus client. It
keeps unbounded decision history and
has not undergone Byzantine/adversarial or comprehensive crash-recovery testing.
The supported launcher flow is a fresh chain per run. It does not implement
validator membership changes, staking, slashing, production key management, or
settlement. Do not connect it to an existing Arkiv network.

## Verification

Verified locally on 2026-09-09 with Arkiv/reth v2.5.0:

- Four equal-weight validators finalize identical block hashes and state roots.
- Writes submitted to every node become visible at a common finalized height.
- Extending expiry preserves the entity past its original expiry; subsequent
  physical purge is observed on all four nodes.
- Three validators continue when one node stops; that node recovers after restart.
- Two validators cannot advance finality.

Regression tests cover accepting only Engine API `VALID`, payload-digest
integrity, round-recovery message encoding, and persistence/rejection of conflicting durable decisions:

```sh
cargo test --locked --manifest-path devnet/malachite/Cargo.toml --workspace
cargo clippy --locked --manifest-path devnet/malachite/Cargo.toml --workspace --all-targets -- -D warnings
```
