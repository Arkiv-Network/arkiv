// A devnet brought up with ARKIV_SEED_COUNT set starts with that many entities
// already in its genesis state. These tests read the seed's manifest (written
// by `scripts/kurtosis/up.py`) and check both nodes serve it from block 0.
//
// Skipped when there is no manifest: an unseeded devnet has nothing to check.

import { beforeAll, describe, expect, test } from "bun:test"
import { existsSync, readFileSync } from "node:fs"
import { resolve } from "node:path"
import { FOLLOWER_RPC_URL, RPC_URL, connect, rpc, type Devnet } from "../src/devnet"

type Manifest = {
  chainId: number
  count: number
  payloadSize: number
  contentType: string
  owners: string[]
  expiresAt: number
  stateRoot: string
  accounts: number
  sampleKeys: string[]
  ownerNonces: Record<string, number>
}

type Entity = {
  key: string
  owner: string
  creator: string
  createdAt: string
  expiresAt: string
  contentType: string
  payload: string
  attributes: { name: string; type: string; value: string }[]
}

const MANIFEST_PATH = resolve(
  import.meta.dir,
  process.env.ARKIV_SEED_MANIFEST ?? "../../.seed/manifest.json",
)
const manifest: Manifest | null = existsSync(MANIFEST_PATH)
  ? (JSON.parse(readFileSync(MANIFEST_PATH, "utf8")) as Manifest)
  : null

const describeSeeded = manifest ? describe : describe.skip
// keccak256 of the RLP-encoded empty trie: the state root of a genesis with no accounts at all.
const EMPTY_ROOT = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"

let devnet: Devnet

beforeAll(async () => {
  devnet = await connect()
})

describeSeeded("seeded genesis", () => {
  const seed = manifest as Manifest

  test("the manifest describes this chain", async () => {
    expect(await devnet.publicClient.getChainId()).toBe(seed.chainId)
    expect(seed.sampleKeys.length).toBeGreaterThan(0)
  })

  test("block 0 is a seeded genesis, not an empty one", async () => {
    // The manifest's stateRoot is the root of the seeded alloc alone;
    // ethereum-package merges that alloc with its own prefunded accounts and
    // system contracts, so block 0's root is a different one. What must hold
    // is that the seed is already there at block 0.
    const genesis = await devnet.publicClient.getBlock({ blockNumber: 0n })
    expect(genesis.stateRoot).not.toBe(EMPTY_ROOT)
    expect(await rpc<number>("arkiv_getEntityCount", [{ block: 0 }])).toBe(seed.count)
  })

  test("every seeded entity is counted at block 0 and still at the tip", async () => {
    expect(await rpc<number>("arkiv_getEntityCount", [{ block: 0 }])).toBe(seed.count)
    expect(await rpc<number>("arkiv_getEntityCount", [{}])).toBeGreaterThanOrEqual(seed.count)
  })

  test("a seeded entity reads back with its owner, payload and attributes", async () => {
    const entity = await rpc<Entity>("arkiv_getEntity", [seed.sampleKeys[0]])
    expect(entity).not.toBeNull()
    expect(entity.key.toLowerCase()).toBe(seed.sampleKeys[0].toLowerCase())
    expect(entity.owner.toLowerCase()).toBe(seed.owners[0].toLowerCase())
    expect(entity.creator.toLowerCase()).toBe(seed.owners[0].toLowerCase())
    expect(Number(entity.createdAt)).toBe(0)
    expect(entity.contentType).toBe(seed.contentType)
    expect(entity.payload.length).toBe(2 + 2 * seed.payloadSize)
    expect(entity.attributes.map((a) => a.name)).toEqual(["rank", "team"])
  })

  test("seeded entities are queryable through the index", async () => {
    // rank = i mod 100 and team cycles three names, so each bucket is
    // populated once the seed holds a few hundred entities.
    const perRank = Math.floor(seed.count / 100)
    if (perRank > 0) {
      expect(await rpc<number>("arkiv_getEntityCount", [{ query: "rank = u256(7)" }])).toBe(
        perRank + (seed.count % 100 > 7 ? 1 : 0),
      )
    }
    const owner = seed.owners[0].toLowerCase()
    const owned = await rpc<number>("arkiv_getEntityCount", [{ query: `$owner = addr(${owner})` }])
    expect(owned).toBeGreaterThanOrEqual(Math.floor(seed.count / seed.owners.length))

    const page = await rpc<{ data: { key: string }[]; cursor?: string }>("arkiv_query", [
      "team = str('red')",
      { limit: 10 },
    ])
    expect(page.data.length).toBeGreaterThan(0)
    expect(page.data.length).toBeLessThanOrEqual(10)
  })

  test("the seeded owner's minting nonce continues from the seed", async () => {
    const owner = seed.owners[0]
    // entityNonce(address) on the registry, as the SDK calls it before a create.
    const selector = "0x36917bfd" // IEntityRegistry.entityNonce(address), pinned in arkiv-bindings
    const data = selector + owner.toLowerCase().replace("0x", "").padStart(64, "0")
    const result = await rpc<string>("eth_call", [
      { to: "0x4400000000000000000000000000000000000044", data },
      "latest",
    ])
    expect(BigInt(result)).toBe(BigInt(seed.ownerNonces[owner] ?? seed.ownerNonces[owner.toLowerCase()]))
  })

  test("the follower serves the same seeded genesis as the sequencer", async () => {
    const [sequencer, follower] = await Promise.all([
      rpc<{ hash: string; stateRoot: string }>("eth_getBlockByNumber", ["0x0", false], RPC_URL),
      rpc<{ hash: string; stateRoot: string }>("eth_getBlockByNumber", ["0x0", false], FOLLOWER_RPC_URL),
    ])
    expect(follower.hash).toBe(sequencer.hash)
    expect(follower.stateRoot).toBe(sequencer.stateRoot)
    expect(await rpc<number>("arkiv_getEntityCount", [{ block: 0 }], FOLLOWER_RPC_URL)).toBe(seed.count)
    const entity = await rpc<Entity>("arkiv_getEntity", [seed.sampleKeys[1]], FOLLOWER_RPC_URL)
    expect(entity.owner.toLowerCase()).toBe(seed.owners[1 % seed.owners.length].toLowerCase())
  })
})
