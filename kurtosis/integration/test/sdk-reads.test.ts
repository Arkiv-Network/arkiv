// The SDK's read-only Arkiv namespace against the devnet: the calls whose wire
// shape this node and SDK 0.7 agree on.

import { beforeAll, describe, expect, test } from "bun:test"
import { connect, type Devnet } from "../src/devnet"

let devnet: Devnet

beforeAll(async () => {
  devnet = await connect()
})

describe("arkiv_* namespace through the SDK", () => {
  test("the chain id the SDK sees is the one the node reports", async () => {
    expect(await devnet.publicClient.getChainId()).toBe(devnet.chain.id)
  })

  test("arkiv_getBlockTiming tracks the chain tip", async () => {
    const timing = await devnet.publicClient.getBlockTiming()
    const head = await devnet.publicClient.getBlockNumber()
    expect(timing.currentBlock).toBeLessThanOrEqual(head)
    expect(head - timing.currentBlock).toBeLessThanOrEqual(2n)
    expect(timing.currentBlockTime).toBeGreaterThan(0)
  })

  test("arkiv_getEntityCount answers on an empty store", async () => {
    const count = await devnet.publicClient.getEntityCount()
    expect(Number(count)).toBeGreaterThanOrEqual(0)
  })
})
