# Demo: a chain on GolemDB, from nothing, in one command

`run.py` starts a throwaway Valkey, runs `arkiv-reth --dev` against it for a few seconds, sends one transaction, collects the evidence, and removes everything it started.

```sh
cargo build --bin arkiv-reth --profile dev-fast
scripts/demo/run.py            # 30 seconds
scripts/demo/run.py 10         # or however long
```

Nothing it starts is shared. Valkey gets a free port and its own data directory, snapshotting off; the chain gets its own datadir; the store namespace carries the run's timestamp. A Valkey you already run on 6379 is untouched, and two runs can overlap.

| Flag / variable | Meaning |
| --- | --- |
| `seconds` | How long to let the chain run (default 30) |
| `--out DIR` | Where to write the bundle (default `target/demo/<timestamp>`) |
| `--keep` | Leave Valkey and the chain up, and print how to reach them |
| `--restart` | Stop the node and start it again against the same store, and check it comes back where the store says the chain is |
| `ARKIV_RETH_BINARY` | Use this binary instead of looking in `target/{dev-fast,debug,release}` |

Foundry is optional. With `cast` on PATH the run sends one transfer so the chain has something to execute; without it the chain just idles and the summary says so.

## What you get

One directory, named in the last line of output:

| File | What it holds |
| --- | --- |
| `node.log` | Everything arkiv-reth wrote, stdout and stderr |
| `valkey.log` | The same for the Valkey server |
| `progress.json` | Block height, entity count and key count, sampled ten times while running |
| `valkey-keys.txt` | Every key the run left in the store |
| `store-summary.txt` | Key totals by kind, and the cells of a record if there is one |
| `transfer.log` | `cast send` output, when foundry is installed |

## What the store actually holds

A plain transfer leaves **no entity records**, which is the design rather than a fault: entity records are written by Arkiv operations, and a transfer is not one. It does leave an account record, because balances and nonces are GolemDB's too since the accounts cutover. So an idle or transfer-only run holds commit bookkeeping and accounts, and no entities:

```
by kind:
     1  branch
     1  branches
    22  commit
     1  digest
     1  head
     1  nextbranch
    11  tag
```

One `commit:N:hash` and one `tag:` per block — the commit lineage, one commit per block, which is the thing worth seeing: the chain is committing to GolemDB as it advances.

`rec:` keys appear once something **creates an entity**. The quickest way to see them, and the cell names with them, is the e2e suite pointed at a Valkey:

```sh
ARKIV_EXPERIMENTAL_STORE=redis://127.0.0.1:6379 ARKIV_STORE_NAMESPACE=scratch \
  cargo test -p arkiv-reth --test e2e query_operator_classes_over_a_live_node
```

When a record is present the summary prints its cell names, which is where the prefix split shows: `$`-prefixed cells are Arkiv's, under the one prefix GolemDB's grammar leaves to a host protocol, and `#v` is the engine's own.

## Restart

`--restart` is the one that shows why any of this exists. The run stops the node with SIGTERM, leaves Valkey up, starts the node again on the same datadir and namespace, and then checks three things:

- it resumes at the height it stopped at, rather than at whatever reth last flushed to its own database;
- the transfer recipient still holds its ether, which is GolemDB's record and not reth's;
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

This is the graceful stop, the one an operator performs. The abrupt one — SIGKILL — is covered by `arkiv-reth::recovery`, which needs a Valkey of its own.

## Exit status

Zero when the chain advanced and the store is non-empty. Non-zero, with the reason, when the chain never moved, when the store came back empty, or when Valkey or the RPC never came up. The logs are written either way, which is the point on a failure.

Ctrl-C is handled: the processes and scratch data still go, the log directory still stays.

## Notes

`--http.api` does not accept `arkiv`. The `arkiv_*` namespace is merged through `extend_rpc_modules`, so it attaches to whichever transports are already enabled rather than being selectable by name — passing it fails the node at startup with "Unknown RPC module".
