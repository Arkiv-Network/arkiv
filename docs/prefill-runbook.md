# Prefill runbook

How to launch an Arkiv chain whose block 0 already holds entities, and how other nodes join it.

A prefill writes entities from archive files into GolemDB **before** the chain starts, so they are part of block 0: created at block 0, with expiries counted from block 0. The operator runs it once, on one machine. Everyone else joins after the chain has started.

```text
arkiv-reth prefill --chain genesis.json --archives set.json              # 1. the archives; resumable
arkiv-reth prefill --chain genesis.json --archives set.json --finalize   # 2. block 0, and its root into genesis.json
arkiv-reth node    --chain genesis.json                                  # 3. block 1 onwards
```

`--store` (or `ARKIV_STORE_PATH`, as for the node) is GolemDB's directory. It must be an absolute path.

## Try it on the fixture

The repository has a small prefill, `crates/arkiv-prefill/fixtures/small`: two archives, six entities. From the repository root, build `arkiv-reth` and put it on the `PATH`:

```bash
just build && export PATH="$PWD/target/debug:$PATH"
```

```bash
cp -R crates/arkiv-prefill/fixtures/small /tmp/prefill && cd /tmp/prefill
```

```bash
arkiv-reth prefill --chain genesis.json --archives set.json --store /tmp/prefill/golemdb
```

```bash
arkiv-reth prefill --chain genesis.json --archives set.json --store /tmp/prefill/golemdb --finalize
```

The second command prints block 0's state root and hash, and writes the root into `genesis.json`. Both match `expected.json`. Then start a dev node on that store:

```bash
ARKIV_STORE_PATH=/tmp/prefill/golemdb arkiv-reth node --chain genesis.json --datadir /tmp/prefill/reth --dev --dev.block-time 1s --http
```

`eth_getBlockByNumber("0x0")` returns that hash, and `arkiv_getEntity` returns each prefilled entity with `createdAt` `0x0`.

## 1. Prepare the inputs

- **The genesis file.** A normal Arkiv genesis file (see [genesis-spec.md](genesis-spec.md)), without `prefilledBlockZeroStateRoot`: the prefill writes it. Settle it before you start. The prefill records a hash of the file, and any later change to it (an `alloc` balance, a price, the chain id) is refused, at a resume and at node start. Write a `terminalTotalDifficulty` above 2^64 as a hex string, since the prefill cannot write back such a number as it was.
- **The archives.** Each archive is a directory with an index file and one or more JSON Lines parts; each archive becomes one GolemDB commit. Keep each archive to at most a few hundred megabytes: the prefill holds one archive's writes in memory until it commits it. The format (fields, attribute tags, limits) is in the [`arkiv-prefill` crate docs](../crates/arkiv-prefill/src/lib.rs).
- **The set file.** It lists the archives' index files in commit order, relative to the set file, which keeps the whole prefill in one directory:

  ```json
  {"version": 1, "archives": ["A0/index.json", "A1/index.json", "A2/index.json"]}
  ```

The genesis file and the archives fix block 0 completely: the same files give the same root and hash on any machine.

## 2. Run the prefill

```bash
arkiv-reth prefill --chain genesis.json --archives set.json --store /var/lib/arkiv/golemdb
```

It writes `alloc` and a prefill record as one commit, then one commit per archive, printing each as it lands. Entities go through the same executor as transactions, so an archive the chain would not accept is refused. Prefilled entities cost nothing: no fee is charged and nothing is credited to the storage pool.

**Resuming.** If it stops (crash, Ctrl-C, a refused archive), run the same command again. It continues after the last committed archive. Before it does, it checks that nothing changed: the genesis file's hash, the number of archives, and the hash of the last committed archive. Fix a refused archive only if it has not been committed yet; any change to a committed one means starting again in an empty directory.

Running it again after it finished writes nothing; it only checks.

## 3. Finalize block 0

```bash
arkiv-reth prefill --chain genesis.json --archives set.json --store /var/lib/arkiv/golemdb --finalize
```

It runs the same checks as a resume, then commits block 0 on top of the archives, takes GolemDB's root, and writes it into the genesis file as `config.arkiv.prefilledBlockZeroStateRoot` (replacing the file in one step). It prints the root and block 0's hash. A store can be finalized once; a second `--finalize` is refused.

**If block 0 is committed but the genesis file is not written** (a full disk, a read-only file, a crash in between), don't prefill again. If the command got as far as the write, its error names the root. Otherwise run the same `--finalize` again: the refusal reads `already finalized: block 0's root is 0x…, its hash 0x…`. Add that root under `config.arkiv` by hand, as `"prefilledBlockZeroStateRoot": "0x…"`. The node checks the root against the store at start, so a typo is refused rather than run.

**Before starting the node, keep a copy of the store.** This copy is what joining nodes start from (step 6), and what to publish: anyone can check it against the genesis file. A later store of the same chain also works for a joiner, which skips the blocks the store already holds, but copy it only from a stopped node (a running node's directory can be copied mid-write), and CI checks only the block 0 copy.

```bash
cp -R /var/lib/arkiv/golemdb /var/lib/arkiv/golemdb-block0
```

## 4. Optional: audit

Anyone can check the published root from the published inputs, on another machine and into an empty store:

```bash
arkiv-reth prefill --chain genesis.json --archives set.json --store /tmp/audit
```

```bash
arkiv-reth prefill --chain genesis.json --archives set.json --store /tmp/audit --finalize
```

With the finalized genesis file, `--finalize` does not rewrite it: it refuses unless the root it gets equals the file's, and prints `it matches …'s prefilledBlockZeroStateRoot` when it does.

## 5. Start the chain and publish

```bash
ARKIV_STORE_PATH=/var/lib/arkiv/golemdb arkiv-reth node --chain genesis.json …
```

The node takes block 0 from the field, checks that the store's block 0 has that root, and produces block 1 onwards as usual.

Publish:

| What | For whom |
|---|---|
| The finalized `genesis.json` | everyone: it names the chain (block 0's hash) without the data, and is what the consensus client's genesis is built from |
| The block 0 store copy (`golemdb-block0`) | nodes joining the chain |
| The archives and the set file | auditors |

## 6. Join the chain

A new node joins after the chain has started:

1. Copy the published block 0 store to the node's GolemDB directory.
2. Start `arkiv-reth node --chain genesis.json` with `ARKIV_STORE_PATH` set to it and the chain's peers. At start the node checks the store against the genesis file and refuses a wrong or incomplete copy (see below) before syncing anything.
3. It syncs blocks 1 onwards from its peers.

Running the prefill again from the archives gives the same store, so it is another way to get one, but that is the audit's job; copying is quicker.

## When the node refuses to start

The node checks its store against the genesis file before anything else. Each refusal names what is wrong:

| The error says | Why | What to do |
|---|---|---|
| … the Arkiv store is empty. Run `arkiv-reth prefill` into this store first | The genesis file is prefilled; the store directory is empty or wrong | Point `ARKIV_STORE_PATH` at the prefilled store, or copy it in |
| … an unfinished prefill: N of M archives are in … | The prefill stopped part way | Run `arkiv-reth prefill` again to resume |
| … a finished prefill whose block 0 is not finalized yet … | Every archive is in, `--finalize` was not run | Run `arkiv-reth prefill --finalize` |
| … the genesis file changed after the prefill … | The file was edited after the prefill started. A prefilled block 0 takes its root from the field, so an edit to `alloc` would otherwise go unnoticed | Use the genesis file the prefill finalized |
| … sets config.arkiv.prefilledBlockZeroStateRoot, but the Arkiv store holds no prefill | A store of a chain that was not prefilled | Use the store the prefill finalized |
| … was prefilled, but the genesis file has no config.arkiv.prefilledBlockZeroStateRoot … | The genesis file from before `--finalize` | Use the genesis file the prefill finalized |
| … finalized as another block 0 than this genesis file's … | The store and the file come from different prefills | Use the matching file, or a new store if this one belongs to another chain |
| … the prefill record does not fit its commits … | The store is damaged | Prefill again into a new store |
| … holds another chain … | A store of another chain | Use a new GolemDB directory |
| open GolemDB at …: it was created with other GolemDB settings … | The store was made with other `config.arkiv.golemdb` settings | Use the genesis file the store was made with, or a new directory |

## Reference

- [genesis-spec.md, `prefilledBlockZeroStateRoot`](genesis-spec.md#7-prefilledblockzerostateroot): the genesis field.
- [`arkiv-prefill` crate docs](../crates/arkiv-prefill/src/lib.rs): the archive format and the archive hash.
- Tracking issue: [#311](https://github.com/Arkiv-Network/arkiv/issues/311).
