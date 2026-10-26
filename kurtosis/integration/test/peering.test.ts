// The producer and the follower are peers over reth's own p2p network, not
// only through their consensus clients. Without that, a transaction sent to any
// node but the producer is never mined, and a node that falls behind cannot
// download the blocks it missed.

import { beforeAll, describe, expect, test } from "bun:test"
import { createPublicClient, createWalletClient, http } from "viem"
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts"
import { connect, FOLLOWER_RPC_URL, type Devnet } from "../src/devnet"

let devnet: Devnet

beforeAll(async () => {
  devnet = await connect()
})

describe("the producer and the follower are peers", () => {
  test("a transfer sent to the follower is mined by the producer", async () => {
    const transport = http(FOLLOWER_RPC_URL)
    const follower = createPublicClient({ chain: devnet.chain, transport })
    const sender = privateKeyToAccount(generatePrivateKey())
    const recipient = privateKeyToAccount(generatePrivateKey()).address

    // A fresh sender, funded through the producer, once the follower has the
    // funding block: its nonce is then 0 on both nodes.
    const funding = await devnet.fundedWallet.sendTransaction({ to: sender.address, value: 10n ** 18n })
    await follower.waitForTransactionReceipt({ hash: funding })

    const wallet = createWalletClient({ chain: devnet.chain, transport, account: sender })
    const hash = await wallet.sendTransaction({ to: recipient, value: 1n })
    const receipt = await devnet.publicClient.waitForTransactionReceipt({ hash })
    expect(receipt.status).toBe("success")
    expect(await devnet.publicClient.getBalance({ address: recipient })).toBe(1n)
  }, 60_000)
})
