// Creates entities of three sizes and three lifetimes through SDK-next and prints what each one
// cost: the SDK's gas limit, the gas used, how much of it was storage gas (paid into the pool at
// ARKIV_ADDRESS), and the transaction fee. Ends with an overview of the 9 combinations.
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
// there. COUNT entities per combination (default 3). ONLY limits the run to some combinations,
// e.g. ONLY=big/long,normal/short.
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
import { privateKeyToAccount } from "viem/accounts"

const RPC_URL = process.env.ARKIV_RPC_URL ?? "http://127.0.0.1:8545"
const FUNDED_KEY = (process.env.ARKIV_FUNDED_KEY ??
  "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80") as Hex
const GENESIS = resolve(process.env.GENESIS ?? `${import.meta.dir}/../../../genesis/dev.json`)
const COUNT = Number(process.env.COUNT ?? 3)
const ONLY = process.env.ONLY?.split(",").map((s) => s.trim())

// The pool storage gas is paid into: `base_fee × storage_gas` per transaction.
const POOL = "0x4400000000000000000000000000000000000044"

const genesis = await Bun.file(GENESIS).json()
const MAX_LIFETIME: number = genesis.config.arkiv.tokenomics.max_lifetime

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

type Kind = {
  name: string
  schema: Schema
  values: (i: number) => Record<string, unknown>
  cells: number
  indexed: number
  bodyBytes: number
}

const KINDS: Kind[] = [
  { name: "minimal", schema: Minimal, values: minimalValues, cells: 1, indexed: 0, bodyBytes: 0 },
  {
    name: "normal",
    schema: Normal,
    values: (i) => normalValues(i, account.address),
    cells: 11,
    indexed: 4,
    bodyBytes: NORMAL_BODY,
  },
  {
    name: "big",
    schema: Big,
    values: (i) => bigValues(i, account.address),
    cells: 21,
    indexed: 15,
    bodyBytes: BIG_BODY,
  },
]

type Row = {
  gasLimit: bigint
  gasUsed: bigint
  storage: bigint
  fee: bigint
  entityKey: Hex
  values: Record<string, unknown>
}

// One create, with the pool read at `latest` around it: GolemDB serves no past state, and
// nothing else pays into the pool on a private dev node.
async function create(kind: Kind, expiration: Lifetime, i: number): Promise<Row> {
  const values = kind.values(i)
  const before = await publicClient.getBalance({ address: POOL })
  const { entityKey, txHash } = await wallet.createEntity(kind.schema, values as never, { expiration })
  const after = await publicClient.getBalance({ address: POOL })
  const receipt = await publicClient.getTransactionReceipt({ hash: txHash })
  if (receipt.status !== "success") throw new Error(`${txHash} ${receipt.status}`)
  const block = await publicClient.getBlock({ blockNumber: receipt.blockNumber })
  const tx = await publicClient.getTransaction({ hash: txHash })
  const baseFee = block.baseFeePerGas ?? 0n
  if (baseFee === 0n) throw new Error(`block ${receipt.blockNumber} has no base fee`)
  return {
    gasLimit: tx.gas,
    gasUsed: receipt.gasUsed,
    storage: (after - before) / baseFee,
    fee: receipt.gasUsed * receipt.effectiveGasPrice,
    entityKey,
    values,
  }
}

// A failed create in one line. The SDK names a plain `Error(string)` revert only as "Error", so
// the reason is taken from the revert in its cause chain; a transaction hash means it was mined
// and its gas paid.
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
// Wei as GLM, to the gwei.
const glm = (wei: bigint) => (Number(wei / 1_000_000_000n) / 1e9).toFixed(9)
const line = (cells: [string, number][]) =>
  cells.map(([text, width]) => (width < 0 ? text.padEnd(-width) : text.padStart(width))).join(" ")

console.log(
  `node ${RPC_URL}, chain ${chainId}, sender ${account.address}\n` +
    `schedule from ${GENESIS}: ${JSON.stringify(genesis.config.arkiv.tokenomics)}\n` +
    `creates per kind and lifetime: ${COUNT}\n`,
)
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

// Averages over the creates that succeeded; the fee is what the sender paid, gasUsed × the
// effective gas price, of which `storage × base fee` went to the pool.
console.log("\noverview, average per create")
console.log(
  line([["kind", -8], ["lifetime", -8], ["blocks", 9], ["gasUsed", 10], ["storage", 10], ["other", 10],
    ["tx fee (GLM)", 13], ["ok", 5]]),
)
for (const kind of KINDS) {
  for (const lifetime of LIFETIMES) {
    if (ONLY && !ONLY.includes(`${kind.name}/${lifetime.name}`)) continue
    const rows = results.get(`${kind.name}/${lifetime.name}`) ?? []
    if (rows.length === 0) {
      console.log(line([[kind.name, -8], [lifetime.name, -8], [fmt(lifetime.blocks), 9], ["no successful create", 45]]))
      continue
    }
    const avg = (pick: (r: Row) => bigint) => rows.reduce((sum, r) => sum + pick(r), 0n) / BigInt(rows.length)
    console.log(
      line([[kind.name, -8], [lifetime.name, -8], [fmt(lifetime.blocks), 9], [fmt(avg((r) => r.gasUsed)), 10],
        [fmt(avg((r) => r.storage)), 10], [fmt(avg((r) => r.gasUsed - r.storage)), 10],
        [glm(avg((r) => r.fee)), 13], [`${rows.length}/${COUNT}`, 5]]),
    )
  }
}

console.log("\nread back, first entity of each kind")
let mismatches = 0
for (const kind of KINDS) {
  const row = LIFETIMES.map((l) => results.get(`${kind.name}/${l.name}`)?.[0]).find((r) => r)
  if (!row) continue
  const wrong = await readBack(kind, row)
  mismatches += wrong.length
  console.log(`${kind.name.padEnd(8)} ${wrong.length === 0 ? "matches what was written" : `differs in ${wrong.join(", ")}`}`)
}
process.exit(mismatches === 0 ? 0 : 1)
