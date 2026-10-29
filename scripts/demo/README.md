# Demo: a chain on GolemDB, from nothing, in one command

`run.py` runs `arkiv-reth --dev` on GolemDB for a few seconds, creates and patches one entity and sends one transfer, collects the evidence, and removes everything it started. GolemDB runs inside the node, so there is nothing else to start.

```sh
cargo build --bin arkiv-reth --bin arkiv-cli --profile dev-fast
scripts/demo/run.py --restart  # 30 seconds, then a stop and a start
scripts/demo/run.py 10         # or however long, without the restart
```

Nothing it starts is shared: the chain and GolemDB get their own directories, so two runs can overlap.

| Flag / variable | Meaning |
| --- | --- |
| `seconds` | How long to let the chain run (default 30) |
| `--out DIR` | Where to write the bundle (default `target/demo/<timestamp>`) |
| `--keep` | Leave the store and the chain up, and print how to reach them |
| `--restart` | Stop the node and start it again against the same store, and check it comes back where the store says the chain is |
| `ARKIV_RETH_BINARY` | Use this binary instead of looking in `target/{dev-fast,debug,release}` |
| `ARKIV_CLI_BINARY` | The same for `arkiv-cli`, which creates and patches the entity |

Foundry is optional. With `cast` on PATH the run sends one transfer so the chain has something to execute; without it the summary says so. The entity needs an `arkiv-cli` binary, built like `arkiv-reth`.

## What you get

One directory, named in the last line of output:

| File | What it holds |
| --- | --- |
| `node.log` | Everything arkiv-reth wrote, stdout and stderr |
| `progress.json` | Block height, sampled ten times while running |
| `store-summary.txt` | GolemDB's files, and the entity read back |
| `entity.log` | The `arkiv-cli` create and patch output |
| `restart.json` | With `--restart`: block, balance and entity before and after |
| `transfer.log` | `cast send` output, when foundry is installed |

## Restart

`--restart` is the one that shows why any of this exists. The run stops the node with SIGTERM, leaves the store in place, starts the node again on the same datadir and store, and then checks three things:

- it resumes at the height it stopped at, rather than at whatever reth last flushed to its own database;
- the transfer recipient still holds its ether, which is GolemDB's record and not reth's;
- the entity reads back unchanged, patched payload included;
- it goes on building new blocks, because coming back up and then stalling is not recovery.

`restart.json` in the bundle has the before and after numbers. A typical run:

```json
{
  "block_before_shutdown": 16,
  "block_after_restart": 16,
  "block_after_running_on": 18,
  "recipient_balance_before_shutdown": "0xde0b6b3a7640000",
  "recipient_balance_after_restart": "0xde0b6b3a7640000"
}
```

Without `cast` on PATH nothing is ever spent, so the balance check compares zero with zero; the run says so rather than letting it read as though it proved something.

This is the graceful stop, the one an operator performs. The abrupt one — SIGKILL — is covered by `arkiv-reth::recovery`.

## Exit status

Zero when the chain advanced and the store is non-empty. Non-zero, with the reason, when the chain never moved, when the store came back empty, when the entity does not read back, or when the store or the RPC never came up. The logs are written either way, which is the point on a failure.

Ctrl-C is handled: the processes and scratch data still go, the log directory still stays.

## Notes

`--http.api` does not accept `arkiv`. The `arkiv_*` namespace is merged through `extend_rpc_modules`, so it attaches to whichever transports are already enabled rather than being selectable by name — passing it fails the node at startup with "Unknown RPC module".
