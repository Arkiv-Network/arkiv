// What entity operations cost through SDK-next, in three sections (SECTIONS, default all):
//
// - creates: entities of three sizes and three lifetimes, each with the SDK's gas limit, the gas
//   used, how much of it was storage gas (paid into the pool at ARKIV_ADDRESS), the fee, and how
//   many of it fit in a block. Ends with an overview of the 9 combinations.
// - ops: per kind, a patch, an extend by a week, a transfer and a delete, measured the same way.
// - block: many senders send creates, patches and extends at once, and a table shows how many of
//   each landed in which block and how full each block got.
//
// Not a test: it asserts nothing about prices, so it runs against any fee schedule. See
// docs/sdk-next-testing.md for the node to run it against.
//
//   bun experiment/tokenomics.ts
//
// Run on its own: the name has no `.test.ts`, so `bun test` leaves it out.
//
// ARKIV_RPC_URL and ARKIV_FUNDED_KEY as for the tests (defaults: a local `--dev` node and its
// first hardhat key). GENESIS is the genesis file the node runs on (default genesis/dev.json):
// the node does not serve its fee schedule, so the longest lifetime, `max_lifetime`, is read from
// there. COUNT entities per combination (default 3). ONLY limits the creates to some combinations,
// e.g. ONLY=big/long,normal/short. GLM_USD prices the fees in dollars (default 0.1).
//
// The block section sends from hardhat accounts 1 to BLOCK_SENDERS (default 19) of the dev
// mnemonic, which the dev genesis funds; each sends BLOCK_CREATES, BLOCK_PATCHES and
// BLOCK_EXTENDS of the normal kind (default 5 each). reth's pool guarantees 16 pending
// transactions per sender, so the three together may not exceed that.
//
// The schemas are unnamed: a named one adds an indexed `arkiv.schema` attribute to every entity,
// which would change the counts below. Every kind carries exactly the attributes listed.

import {
  addr,
  createPublicClient,
  createWalletClient,
  defineSchema,
  ExpirationTime,
  i32,
  str,
  u64,
  bytes as bytesColumn,
  type Lifetime,
  type Schema,
} from "@arkiv-network/sdk-next"
import { resolve } from "node:path"
import { ContractFunctionRevertedError, defineChain, http, toHex, type Hex } from "viem"
import { mnemonicToAccount, privateKeyToAccount } from "viem/accounts"

const RPC_URL = process.env.ARKIV_RPC_URL ?? "http://127.0.0.1:8545"
const FUNDED_KEY = (process.env.ARKIV_FUNDED_KEY ??
  "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80") as Hex
const GENESIS = resolve(process.env.GENESIS ?? `${import.meta.dir}/../../../genesis/dev.json`)
const COUNT = Number(process.env.COUNT ?? 3)
const ONLY = process.env.ONLY?.split(",").map((s) => s.trim())
const SECTIONS = (process.env.SECTIONS ?? "creates,ops,block").split(",").map((s) => s.trim())
const GLM_USD = Number(process.env.GLM_USD ?? 0.1)
const BLOCK_SENDERS = Number(process.env.BLOCK_SENDERS ?? 19)
const BLOCK_CREATES = Number(process.env.BLOCK_CREATES ?? 5)
const BLOCK_PATCHES = Number(process.env.BLOCK_PATCHES ?? 5)
const BLOCK_EXTENDS = Number(process.env.BLOCK_EXTENDS ?? 5)

// The mnemonic of the dev genesis' funded accounts; account 0 is ARKIV_FUNDED_KEY's default.
const DEV_MNEMONIC = "test test test test test test test test test test test junk"

// The pool storage gas is paid into: `base_fee × storage_gas` per transaction.
const POOL = "0x4400000000000000000000000000000000000044"

const genesis = await Bun.file(GENESIS).json()
const WEIGHTS: Record<string, number> = genesis.config.arkiv.golemdb.metering.weights
const MAX_LIFETIME = WEIGHTS["host.arkiv.max_lifetime"]!
const HOST_WEIGHTS = Object.fromEntries(Object.entries(WEIGHTS).filter(([name]) => name.startsWith("host.")))

// In blocks. The SDK turns durations into blocks at a fixed 2 s block time, as the node's
// `--dev.block-time 2s` makes them.
const LIFETIMES: { name: string; blocks: number; expiration: Lifetime }[] = [
  { name: "short", blocks: 100, expiration: ExpirationTime.fromBlocks(100) },
  { name: "medium", blocks: 7 * 24 * 60 * 30, expiration: ExpirationTime.fromWeeks(1) },
  { name: "long", blocks: MAX_LIFETIME, expiration: ExpirationTime.fromBlocks(MAX_LIFETIME) },
]

// 1 attribute, not indexed.
const Minimal = defineSchema({
  note: str(),
})

// 10 attributes, 4 indexed, plus a body of a few hundred bytes.
const Normal = defineSchema({
  category: str().indexed(),
  priority: u64().indexed(),
  author: addr().indexed(),
  status: str().indexed(),
  title: str(),
  summary: str(),
  language: str(),
  source: str(),
  version: i32(),
  size: u64(),
  body: bytesColumn(),
})

// 20 attributes, 15 indexed, plus a body of 100 KiB.
const Big = defineSchema({
  tag01: str().indexed(),
  tag02: str().indexed(),
  tag03: str().indexed(),
  tag04: str().indexed(),
  tag05: str().indexed(),
  tag06: str().indexed(),
  tag07: str().indexed(),
  tag08: str().indexed(),
  num01: u64().indexed(),
  num02: u64().indexed(),
  num03: u64().indexed(),
  num04: u64().indexed(),
  num05: u64().indexed(),
  party1: addr().indexed(),
  party2: addr().indexed(),
  title: str(),
  summary: str(),
  license: str(),
  revision: i32(),
  checksum: u64(),
  body: bytesColumn(),
})

const NORMAL_BODY = 300
const BIG_BODY = 100 * 1024

function randomBytes(length: number): Uint8Array {
  // getRandomValues fills at most 64 KiB per call.
  const out = new Uint8Array(length)
  for (let offset = 0; offset < length; offset += 65536) {
    crypto.getRandomValues(out.subarray(offset, Math.min(offset + 65536, length)))
  }
  return out
}

function minimalValues(i: number) {
  return { note: `minimal entity ${i}` }
}

function normalValues(i: number, author: Hex) {
  return {
    category: "documents",
    priority: u64(BigInt(i % 3)),
    author: addr(author),
    status: i % 2 ? "draft" : "published",
    title: `Normal entity number ${i}`,
    summary: "An entity of the size a typical application writes, with a small body.",
    language: "en",
    source: "experiment/tokenomics.ts",
    version: i32(1),
    size: u64(BigInt(NORMAL_BODY)),
    body: randomBytes(NORMAL_BODY),
  }
}

function bigValues(i: number, author: Hex) {
  return {
    tag01: "alpha",
    tag02: "bravo",
    tag03: "charlie",
    tag04: "delta",
    tag05: "echo",
    tag06: "foxtrot",
    tag07: "golf",
    tag08: `big-${i}`,
    num01: u64(BigInt(i)),
    num02: u64(2n),
    num03: u64(3n),
    num04: u64(4n),
    num05: u64(5n),
    party1: addr(author),
    party2: addr("0x70997970C51812dc3A010C7d01b50e0d17dc79C8"),
    title: `Big entity number ${i}`,
    summary: "Twenty attributes, fifteen of them indexed, and a body of 100 KiB.",
    license: "CC-BY-4.0",
    revision: i32(7),
    checksum: u64(123_456_789n),
    body: randomBytes(BIG_BODY),
  }
}

const transport = http(RPC_URL)
const chainId = await createPublicClient({ transport }).getChainId()
const chain = defineChain({
  id: chainId,
  name: `arkiv-${chainId}`,
  nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
  rpcUrls: { default: { http: [RPC_URL] } },
})
const publicClient = createPublicClient({ chain, transport })
const account = privateKeyToAccount(FUNDED_KEY)
const wallet = createWalletClient({ chain, transport, account })
const BLOCK_GAS_LIMIT = (await publicClient.getBlock()).gasLimit
// An address with no key, to transfer entities to.
const TRANSFER_TO = "0x000000000000000000000000000000000000dEaD"

type Kind = {
  name: string
  schema: Schema
  values: (i: number) => Record<string, unknown>
  // A patch that changes one attribute, and what it changes.
  patch: (i: number) => Record<string, unknown>
  patchLabel: string
  cells: number
  indexed: number
  bodyBytes: number
}

const KINDS: Kind[] = [
  {
    name: "minimal",
    schema: Minimal,
    values: minimalValues,
    patch: (i) => ({ note: `minimal entity ${i}, patched` }),
    patchLabel: "patch note (unindexed)",
    cells: 1,
    indexed: 0,
    bodyBytes: 0,
  },
  {
    name: "normal",
    schema: Normal,
    values: (i) => normalValues(i, account.address),
    patch: (i) => ({ status: `archived-${i}` }),
    patchLabel: "patch status (indexed)",
    cells: 11,
    indexed: 4,
    bodyBytes: NORMAL_BODY,
  },
  {
    name: "big",
    schema: Big,
    values: (i) => bigValues(i, account.address),
    patch: (i) => ({ tag08: `big-${i}-patched` }),
    patchLabel: "patch tag08 (indexed)",
    cells: 21,
    indexed: 15,
    bodyBytes: BIG_BODY,
  },
]
const NORMAL = KINDS[1]!

type Cost = {
  gasLimit: bigint
  gasUsed: bigint
  storage: bigint
  fee: bigint
}

type Row = Cost & {
  entityKey: Hex
  values: Record<string, unknown>
}

// One transaction, with the pool read at `latest` around it: GolemDB serves no past state, and
// nothing else pays into the pool on a private dev node. The fee is what the sender paid,
// gasUsed × the effective gas price, of which `storage × base fee` went to the pool.
async function measure<T extends { txHash: Hex }>(send: () => Promise<T>): Promise<{ result: T; cost: Cost }> {
  const before = await publicClient.getBalance({ address: POOL })
  const result = await send()
  const after = await publicClient.getBalance({ address: POOL })
  const receipt = await publicClient.getTransactionReceipt({ hash: result.txHash })
  if (receipt.status !== "success") throw new Error(`${result.txHash} ${receipt.status}`)
  const block = await publicClient.getBlock({ blockNumber: receipt.blockNumber })
  const tx = await publicClient.getTransaction({ hash: result.txHash })
  const baseFee = block.baseFeePerGas ?? 0n
  if (baseFee === 0n) throw new Error(`block ${receipt.blockNumber} has no base fee`)
  return {
    result,
    cost: {
      gasLimit: tx.gas,
      gasUsed: receipt.gasUsed,
      storage: (after - before) / baseFee,
      fee: receipt.gasUsed * receipt.effectiveGasPrice,
    },
  }
}

async function create(kind: Kind, expiration: Lifetime, i: number): Promise<Row> {
  const values = kind.values(i)
  const { result, cost } = await measure(() =>
    wallet.createEntity(kind.schema, values as never, { expiration }),
  )
  return { ...cost, entityKey: result.entityKey, values }
}

// A failed operation in one line. The SDK names a plain `Error(string)` revert only as "Error",
// so the reason is taken from the revert in its cause chain; a transaction hash means it was
// mined and its gas paid.
function describeFailure(error: unknown): string {
  let message = error instanceof Error ? error.message.split("\n")[0] : String(error)
  for (let e: unknown = error; e instanceof Error; e = e.cause) {
    if (e instanceof ContractFunctionRevertedError && e.reason) message += ` (reason: ${e.reason})`
  }
  const txHash = (error as { txHash?: Hex }).txHash
  return txHash ? `${message}, mined in ${txHash}` : `${message}, not mined`
}

// The written value as the node returns it: bytes as hex, typed values unwrapped.
function normalize(value: unknown): string {
  if (value instanceof Uint8Array) return toHex(value)
  if (value && typeof value === "object" && "value" in value) return normalize(value.value)
  return String(value).toLowerCase()
}

async function readBack(kind: Kind, row: Row): Promise<string[]> {
  const entity = (await publicClient.getEntity(row.entityKey, kind.schema)) as Record<string, unknown>
  return Object.entries(row.values)
    .filter(([name, written]) => normalize(entity[name]) !== normalize(written))
    .map(([name]) => name)
}

const fmt = (n: bigint | number) => n.toLocaleString("en-US")
// Wei as GLM, to the gwei, and in dollars at GLM_USD.
const glm = (wei: bigint) => (Number(wei / 1_000_000_000n) / 1e9).toFixed(9)
const usd = (wei: bigint) => `$${((Number(wei / 1_000_000_000n) / 1e9) * GLM_USD).toFixed(6)}`
// How many of a transaction fit in one block, by gas.
const perBlock = (gasUsed: bigint) => fmt(gasUsed === 0n ? 0n : BLOCK_GAS_LIMIT / gasUsed)
const line = (cells: [string, number][]) =>
  cells.map(([text, width]) => (width < 0 ? text.padEnd(-width) : text.padStart(width))).join(" ")
const avg = <T,>(rows: T[], pick: (r: T) => bigint) =>
  rows.reduce((sum, r) => sum + pick(r), 0n) / BigInt(rows.length)

console.log(
  `node ${RPC_URL}, chain ${chainId}, sender ${account.address}, block gas limit ${fmt(BLOCK_GAS_LIMIT)}\n` +
    `schedule from ${GENESIS}: ${JSON.stringify(HOST_WEIGHTS)}, base fee ${genesis.baseFeePerGas ?? "default"}\n` +
    `sections: ${SECTIONS.join(", ")}; GLM at $${GLM_USD}`,
)

let mismatches = 0

// ---- creates ----

if (SECTIONS.includes("creates")) {
  console.log(`\ncreates, ${COUNT} per kind and lifetime`)
  console.log(
    line([["kind", -8], ["lifetime", -8], ["blocks", 9], ["cells", 5], ["indexed", 7], ["bytes", 9],
      ["gasLimit", 10], ["gasUsed", 10], ["storage", 10], ["other", 10], ["entityKey", -10]]),
  )

  const results = new Map<string, Row[]>()
  for (const kind of KINDS) {
    for (const lifetime of LIFETIMES) {
      if (ONLY && !ONLY.includes(`${kind.name}/${lifetime.name}`)) continue
      const rows: Row[] = []
      for (let i = 0; i < COUNT; i++) {
        try {
          const row = await create(kind, lifetime.expiration, i)
          rows.push(row)
          console.log(
            line([[kind.name, -8], [lifetime.name, -8], [fmt(lifetime.blocks), 9], [String(kind.cells), 5],
              [String(kind.indexed), 7], [fmt(kind.bodyBytes), 9], [fmt(row.gasLimit), 10],
              [fmt(row.gasUsed), 10], [fmt(row.storage), 10], [fmt(row.gasUsed - row.storage), 10],
              [row.entityKey, -10]]),
          )
        } catch (error) {
          console.log(`${kind.name.padEnd(8)} ${lifetime.name.padEnd(8)} create ${i} failed: ${describeFailure(error)}`)
        }
      }
      results.set(`${kind.name}/${lifetime.name}`, rows)
    }
  }

  console.log("\noverview, average per create")
  console.log(
    line([["kind", -8], ["lifetime", -8], ["blocks", 9], ["gasUsed", 10], ["storage", 10], ["other", 10],
      ["per block", 9], ["tx fee (GLM)", 13], ["USD", 10], ["ok", 5]]),
  )
  for (const kind of KINDS) {
    for (const lifetime of LIFETIMES) {
      if (ONLY && !ONLY.includes(`${kind.name}/${lifetime.name}`)) continue
      const rows = results.get(`${kind.name}/${lifetime.name}`) ?? []
      if (rows.length === 0) {
        console.log(line([[kind.name, -8], [lifetime.name, -8], [fmt(lifetime.blocks), 9], ["no successful create", 45]]))
        continue
      }
      const gasUsed = avg(rows, (r) => r.gasUsed)
      const fee = avg(rows, (r) => r.fee)
      console.log(
        line([[kind.name, -8], [lifetime.name, -8], [fmt(lifetime.blocks), 9], [fmt(gasUsed), 10],
          [fmt(avg(rows, (r) => r.storage)), 10], [fmt(avg(rows, (r) => r.gasUsed - r.storage)), 10],
          [perBlock(gasUsed), 9], [glm(fee), 13], [usd(fee), 10], [`${rows.length}/${COUNT}`, 5]]),
      )
    }
  }

  console.log("\nread back, first entity of each kind")
  for (const kind of KINDS) {
    const row = LIFETIMES.map((l) => results.get(`${kind.name}/${l.name}`)?.[0]).find((r) => r)
    if (!row) continue
    const wrong = await readBack(kind, row)
    mismatches += wrong.length
    console.log(`${kind.name.padEnd(8)} ${wrong.length === 0 ? "matches what was written" : `differs in ${wrong.join(", ")}`}`)
  }
}

// ---- ops ----

if (SECTIONS.includes("ops")) {
  console.log("\nops, one each per kind, on entities created for 100 blocks")
  console.log(
    line([["kind", -8], ["op", -24], ["gasLimit", 10], ["gasUsed", 10], ["storage", 10], ["other", 10],
      ["per block", 9], ["tx fee (GLM)", 13], ["USD", 10]]),
  )
  for (const kind of KINDS) {
    const short = { expiration: ExpirationTime.fromBlocks(100) }
    let target: Hex
    let transferred: Hex
    try {
      target = (await wallet.createEntity(kind.schema, kind.values(0) as never, short)).entityKey
      transferred = (await wallet.createEntity(kind.schema, kind.values(1) as never, short)).entityKey
    } catch (error) {
      console.log(`${kind.name.padEnd(8)} setup failed: ${describeFailure(error)}`)
      continue
    }
    const ops: [string, () => Promise<{ txHash: Hex }>][] = [
      [kind.patchLabel, () => wallet.patchEntity(kind.schema, target, { set: kind.patch(0) as never })],
      ["extend +1 week", () => wallet.extendEntity({ entityKey: target, expiration: ExpirationTime.fromWeeks(1) })],
      ["transfer", () => wallet.changeOwnership({ entityKey: transferred, newOwner: TRANSFER_TO })],
      ["delete", () => wallet.deleteEntity({ entityKey: target })],
    ]
    for (const [label, send] of ops) {
      try {
        const { cost } = await measure(send)
        console.log(
          line([[kind.name, -8], [label, -24], [fmt(cost.gasLimit), 10], [fmt(cost.gasUsed), 10],
            [fmt(cost.storage), 10], [fmt(cost.gasUsed - cost.storage), 10], [perBlock(cost.gasUsed), 9],
            [glm(cost.fee), 13], [usd(cost.fee), 10]]),
        )
      } catch (error) {
        console.log(`${kind.name.padEnd(8)} ${label.padEnd(24)} failed: ${describeFailure(error)}`)
      }
    }
  }
}

// ---- block ----

// Many senders at once, so the pool holds more than one block takes: the table shows where the
// block gas limit cut each block off. Every op is a transaction of its own, sent with a gas limit
// 10% above what one of its kind used, so the builder packs them about as tightly as it can.
if (SECTIONS.includes("block")) {
  const perSender = BLOCK_CREATES + BLOCK_PATCHES + BLOCK_EXTENDS
  if (perSender > 16) throw new Error(`${perSender} transactions per sender; reth's pool guarantees 16`)
  const senders = Array.from({ length: BLOCK_SENDERS }, (_, i) =>
    createWalletClient({ chain, transport, account: mnemonicToAccount(DEV_MNEMONIC, { addressIndex: i + 1 }) }),
  )
  const kind = NORMAL
  const week = { expiration: ExpirationTime.fromWeeks(1) }
  console.log(
    `\nblock: ${BLOCK_SENDERS} senders × (${BLOCK_CREATES} creates, ${BLOCK_PATCHES} patches, ` +
      `${BLOCK_EXTENDS} extends) of ${kind.name} entities = ${fmt(BLOCK_SENDERS * perSender)} transactions`,
  )

  // One of each, measured, for the gas limits.
  const probe = senders[0]!
  const probeKey = (await probe.createEntity(kind.schema, kind.values(0) as never, week)).entityKey
  const gasOf = async (send: () => Promise<{ txHash: Hex }>) =>
    (await publicClient.getTransactionReceipt({ hash: (await send()).txHash })).gasUsed
  const limits = {
    create: await gasOf(() => probe.createEntity(kind.schema, kind.values(1) as never, week)),
    patch: await gasOf(() => probe.patchEntity(kind.schema, probeKey, { set: kind.patch(0) as never })),
    extend: await gasOf(() =>
      probe.extendEntity({ entityKey: probeKey, expiration: ExpirationTime.fromWeeks(2) }),
    ),
  }
  for (const op of ["create", "patch", "extend"] as const) limits[op] = (limits[op] * 11n) / 10n
  console.log(`gas limits: create ${fmt(limits.create)}, patch ${fmt(limits.patch)}, extend ${fmt(limits.extend)}`)

  // The entities each sender patches and extends, its own, in one batch per sender.
  const targets = await Promise.all(
    senders.map(async (sender) => {
      const creates = Array.from({ length: BLOCK_PATCHES + BLOCK_EXTENDS }, (_, i) => ({
        schema: kind.schema,
        values: normalValues(i, sender.account.address),
        ...week,
      }))
      if (creates.length === 0) return []
      return (await sender.executeBatch({ creates } as never)).createdEntities
    }),
  )
  console.log(`targets: ${fmt(targets.flat().length)} entities created to patch and extend`)

  // Explicit fees and nonces: the SDK then sends without estimating, so every transaction is in
  // the pool within moments. Two base fees of headroom, as viem would leave.
  const baseFee = (await publicClient.getBlock()).baseFeePerGas ?? 0n
  const tip = 1_000_000_000n
  const fees = { maxFeePerGas: 2n * baseFee + tip, maxPriorityFeePerGas: tip }
  const nonces = await Promise.all(
    senders.map((s) => publicClient.getTransactionCount({ address: s.account.address, blockTag: "pending" })),
  )

  type Sent = { op: string; txHash?: Hex; error?: string }
  const jobs: (() => Promise<Sent>)[] = []
  senders.forEach((sender, s) => {
    let nonce = nonces[s]!
    const own = targets[s]!
    const send = (op: string, gas: bigint, run: (tx: { gas: bigint; nonce: number }) => Promise<{ txHash: Hex }>) => {
      const tx = { gas, nonce: nonce++, ...fees }
      jobs.push(() =>
        run(tx).then(
          (r) => ({ op, txHash: r.txHash }),
          (e) => ({ op, error: describeFailure(e), txHash: (e as { txHash?: Hex }).txHash }),
        ),
      )
    }
    for (let i = 0; i < BLOCK_CREATES; i++) {
      send("create", limits.create, (tx) =>
        sender.createEntity(kind.schema, normalValues(100 + i, sender.account.address) as never, week, tx),
      )
    }
    for (let i = 0; i < BLOCK_PATCHES; i++) {
      send("patch", limits.patch, (tx) =>
        sender.patchEntity(kind.schema, own[i]!, { set: kind.patch(i) as never }, tx),
      )
    }
    for (let i = 0; i < BLOCK_EXTENDS; i++) {
      send("extend", limits.extend, (tx) =>
        sender.extendEntity({ entityKey: own[BLOCK_PATCHES + i]!, expiration: ExpirationTime.fromWeeks(2) }, tx),
      )
    }
  })

  // Start at the beginning of a block, to give the sends the whole slot.
  const start = await publicClient.getBlockNumber()
  while ((await publicClient.getBlockNumber()) === start) await Bun.sleep(50)
  const fired = performance.now()
  const sent = await Promise.all(jobs.map((job) => job()))
  console.log(`all ${fmt(sent.length)} receipts in after ${((performance.now() - fired) / 1000).toFixed(1)} s`)

  const failed = sent.filter((s) => s.error)
  for (const f of failed.slice(0, 3)) console.log(`failed ${f.op}: ${f.error}`)
  if (failed.length > 3) console.log(`… and ${failed.length - 3} more failed`)

  type BlockRow = { creates: number; patches: number; extends: number; gas: bigint }
  const byBlock = new Map<bigint, BlockRow>()
  for (const s of sent.filter((s) => s.txHash)) {
    const receipt = await publicClient.getTransactionReceipt({ hash: s.txHash! })
    const row = byBlock.get(receipt.blockNumber) ?? { creates: 0, patches: 0, extends: 0, gas: 0n }
    if (s.op === "create") row.creates++
    if (s.op === "patch") row.patches++
    if (s.op === "extend") row.extends++
    row.gas += receipt.gasUsed
    byBlock.set(receipt.blockNumber, row)
  }
  console.log(
    line([["block", 7], ["ops", 5], ["creates", 8], ["patches", 8], ["extends", 8], ["block gasUsed", 14],
      ["of limit", 9]]),
  )
  let fullest = { ops: 0, share: 0 }
  for (const number of [...byBlock.keys()].sort((a, b) => Number(a - b))) {
    const row = byBlock.get(number)!
    const block = await publicClient.getBlock({ blockNumber: number })
    const ops = row.creates + row.patches + row.extends
    const share = Number((block.gasUsed * 1000n) / block.gasLimit) / 10
    if (ops > fullest.ops) fullest = { ops, share }
    console.log(
      line([[String(number), 7], [String(ops), 5], [String(row.creates), 8], [String(row.patches), 8],
        [String(row.extends), 8], [fmt(block.gasUsed), 14], [`${share.toFixed(1)}%`, 9]]),
    )
  }
  console.log(
    `${fmt(sent.length - failed.length)} of ${fmt(sent.length)} ops landed in ${byBlock.size} block(s); ` +
      `the fullest held ${fullest.ops} ops at ${fullest.share.toFixed(1)}% of the block gas limit`,
  )
}

process.exit(mismatches === 0 ? 0 : 1)
