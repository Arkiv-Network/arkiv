// A follower stopped while the sequencer moves on, then started again, must
// catch up: reach the sequencer's head with the same blocks, serve state at
// `latest`, and keep following. A restart, a crash and a moved node all leave
// a node behind like this.
//
// It stops and starts the follower's services, so it runs only when
// ARKIV_RESTART_TESTS=1, inside the Kurtosis enclave `scripts/kurtosis/up.py`
// brings up, and after the other tests: a follower that fails to catch up stays
// behind for every test after it.

import { describe, expect, test } from "bun:test"
import { FOLLOWER_RPC_URL, RPC_URL, rpc } from "../src/devnet"

const ENCLAVE = process.env.ARKIV_ENCLAVE ?? "arkiv-harness"
// Participant 2 in kurtosis/arkiv-chain.yaml: its execution and consensus clients.
const FOLLOWER_EL = "el-2-reth-lighthouse"
const FOLLOWER = [FOLLOWER_EL, "cl-2-lighthouse-reth"]
// What the node logs when it refuses a block whose parent is not in its store.
const REFUSAL = "is not in the Arkiv store"
// What reth logs when it starts its pipeline, which never writes GolemDB.
const PIPELINE = "Preparing stage pipeline_stages=1/"
const SLOT_MS = 2_000
const MAX_LAG = 2n
// Lighthouse's `SLOT_IMPORT_TOLERANCE`: within this many slots of its peers it
// fetches the blocks it missed one by one and feeds them to reth; further behind
// it batch-syncs, and with --disable-optimistic-finalized-sync it feeds those
// blocks to reth too. One slot is one block here.
const LIGHTHOUSE_SYNC_TOLERANCE = 32n
// A node more than this many blocks behind the finalized block is past what
// reth syncs on its own: the follower's `--engine.backfill-threshold`, which
// kurtosis/arkiv-chain.yaml lowers from 1024 and says why.
const RETH_SYNC_LIMIT = await followerSyncLimit()
// Blocks per epoch in the `minimal` preset; finality moves one epoch at a time.
const EPOCH = 8n
// Well within Lighthouse's tolerance, with the few blocks made while the
// follower starts again included.
const SHORT_GAP = 10n

function kurtosis(...args: string[]): string {
  const run = Bun.spawnSync(["kurtosis", ...args])
  if (run.exitCode !== 0) throw new Error(`kurtosis ${args.join(" ")}: ${run.stderr.toString()}`)
  return run.stdout.toString()
}

// Read from the chain's args file, so the value is written in one place.
async function followerSyncLimit(): Promise<bigint> {
  const chain = Bun.YAML.parse(await Bun.file(`${import.meta.dir}/../../arkiv-chain.yaml`).text()) as {
    participants: { el_extra_params?: string[] }[]
  }
  const flag = "--engine.backfill-threshold="
  const param = chain.participants[1]?.el_extra_params?.find((p) => p.startsWith(flag))
  if (param === undefined) throw new Error(`kurtosis/arkiv-chain.yaml sets no ${flag} for the follower`)
  return BigInt(param.slice(flag.length))
}

function inEnclave(): boolean {
  try {
    kurtosis("enclave", "inspect", ENCLAVE)
    return true
  } catch {
    return false
  }
}

// The node's height, or undefined while its RPC is not up.
async function head(url: string): Promise<bigint | undefined> {
  try {
    return BigInt(await rpc<string>("eth_blockNumber", [], url))
  } catch {
    return undefined
  }
}

// A gap that leaves the follower past reth's sync limit behind the finalized
// block. Finality trails the head by up to an epoch more than it does now, so
// one epoch on top keeps the gap past the limit whenever it restarts.
async function gapPastRethSync(): Promise<bigint> {
  // A young chain has finalized nothing yet, and the node then has no block to
  // answer "finalized" with.
  const finalized = await until("the chain finalizes a block", 120_000, async () => {
    try {
      const block = await rpc<{ number: string } | null>("eth_getBlockByNumber", ["finalized", false])
      return block ? BigInt(block.number) : undefined
    } catch {
      return undefined
    }
  })
  const latest = await rpc<{ number: string }>("eth_getBlockByNumber", ["latest", false])
  return RETH_SYNC_LIMIT + BigInt(latest.number) - finalized + EPOCH
}

async function hashAt(url: string, height: bigint): Promise<string> {
  const block = await rpc<{ hash: string }>("eth_getBlockByNumber", [`0x${height.toString(16)}`, false], url)
  return block.hash
}

// Poll `check` until it returns a value, failing with `what` after `ms`.
async function until<T>(what: string, ms: number, check: () => Promise<T | undefined>): Promise<T> {
  const deadline = Date.now() + ms
  for (;;) {
    const value = await check()
    if (value !== undefined) return value
    if (Date.now() > deadline) throw new Error(`${what}: not within ${ms / 1000}s`)
    await Bun.sleep(1_000)
  }
}

// The follower's log lines that contain `text`. Colours come off first: reth
// colours field names, so `pipeline_stages=` is not contiguous until then.
function followerLines(text: string): string[] {
  return kurtosis("service", "logs", "--all", ENCLAVE, FOLLOWER_EL)
    .split("\n")
    .map((line) => line.replace(/\x1b\[[0-9;]*m/g, ""))
    .filter((line) => line.includes(text))
}

// Stop `services` while the sequencer makes `gap` blocks, start them again, and
// check the follower catches up.
async function restartAfter(gap: bigint, services = FOLLOWER) {
  const before = await head(FOLLOWER_RPC_URL)
  const start = await head(RPC_URL)
  if (before === undefined || start === undefined) throw new Error("both nodes must answer first")
  if (start - before > MAX_LAG) {
    throw new Error(`the follower is ${start - before} blocks behind before the restart; an earlier case left it behind`)
  }

  kurtosis("service", "stop", ENCLAVE, ...services)
  // Four slots a block leaves room for missed slots without hiding a stopped sequencer.
  await until(`sequencer makes ${gap} blocks`, Number(gap) * SLOT_MS * 4 + 60_000, async () => {
    const latest = await head(RPC_URL)
    return latest !== undefined && latest >= start + gap ? latest : undefined
  })
  kurtosis("service", "start", ENCLAVE, ...services)

  let caughtUp: bigint
  try {
    caughtUp = await until("follower catches up", 120_000, async () => {
      const [sequencer, follower] = [await head(RPC_URL), await head(FOLLOWER_RPC_URL)]
      return sequencer !== undefined && follower !== undefined && sequencer - follower <= MAX_LAG
        ? follower
        : undefined
    })
  } catch (error) {
    const refused = followerLines(REFUSAL)
    const pipelines = followerLines(PIPELINE)
    const detail =
      (refused.length > 0 ? `; it refused ${refused.length} block(s): ${refused.at(-1)}` : "") +
      (pipelines.length > 0 ? `; reth ran its pipeline ${pipelines.length} time(s)` : "")
    throw new Error(`${(error as Error).message}${detail}`)
  }

  // Before the gap, inside it, and at the end.
  for (const height of [before, before + gap / 2n, caughtUp]) {
    expect(await hashAt(FOLLOWER_RPC_URL, height)).toBe(await hashAt(RPC_URL, height))
  }
  // A node that has caught up but cannot read its state is not caught up.
  const read = rpc("eth_getBalance", [`0x${"00".repeat(20)}`, "latest"], FOLLOWER_RPC_URL)
  expect(await Promise.race([read.then(() => "answered"), Bun.sleep(10_000).then(() => "hung")])).toBe("answered")
  expect(followerLines(REFUSAL)).toEqual([])
  expect(followerLines(PIPELINE)).toEqual([])

  // And it keeps following.
  const next = caughtUp + 3n
  await until(`follower reaches block ${next}`, 30_000, async () => {
    const follower = await head(FOLLOWER_RPC_URL)
    return follower !== undefined && follower >= next ? follower : undefined
  })
  expect(await hashAt(FOLLOWER_RPC_URL, next)).toBe(await hashAt(RPC_URL, next))
}

const enabled = process.env.ARKIV_RESTART_TESTS === "1" && inEnclave()

describe.skipIf(!enabled)("a restarted follower catches up", () => {
  test("within Lighthouse's sync tolerance", () => restartAfter(SHORT_GAP), 300_000)

  // One container crashing. Lighthouse stays up and resends the blocks reth
  // missed once it sees reth again, about 30 s later.
  test(
    "with only reth restarted, within Lighthouse's sync tolerance",
    () => restartAfter(SHORT_GAP, [FOLLOWER_EL]),
    300_000,
  )

  // Past Lighthouse's tolerance, so Lighthouse batch-syncs the gap and feeds it
  // to reth block by block.
  test(
    "past Lighthouse's sync tolerance",
    () => restartAfter(LIGHTHOUSE_SYNC_TOLERANCE + EPOCH),
    400_000,
  )

  // Further behind the finalized block than reth syncs on its own: in
  // production, an outage of more than about 37 minutes. Only Lighthouse
  // feeding reth every block closes this gap; reth on its own runs its pipeline
  // and the node stalls.
  test(
    "past what reth syncs on its own",
    async () => restartAfter(await gapPastRethSync()),
    600_000,
  )
})
