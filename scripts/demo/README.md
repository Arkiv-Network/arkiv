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

A plain transfer leaves **no entity records**. That is the design, not a fault: Ethereum balances and nonces live on reth, and only Arkiv's own state goes to GolemDB (`HostStateView`). So an idle or transfer-only run leaves commit bookkeeping and nothing else:

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
ARKIV_STORE_URL=redis://127.0.0.1:6379 ARKIV_STORE_NAMESPACE=scratch \
  cargo test -p arkiv-reth --test e2e query_operator_classes_over_a_live_node
```

When a record is present the summary prints its cell names, which is where the prefix split shows: `$`-prefixed cells are Arkiv's, under the one prefix GolemDB's grammar leaves to a host protocol, and `#v` is the engine's own.

## Exit status

Zero when the chain advanced and the store is non-empty. Non-zero, with the reason, when the chain never moved, when the store came back empty, or when Valkey or the RPC never came up. The logs are written either way, which is the point on a failure.

Ctrl-C is handled: the processes and scratch data still go, the log directory still stays.

## Notes

`--http.api` does not accept `arkiv`. The `arkiv_*` namespace is merged through `extend_rpc_modules`, so it attaches to whichever transports are already enabled rather than being selectable by name — passing it fails the node at startup with "Unknown RPC module".
