// `eth_call` / `eth_estimateGas` run with fee charging disabled, so the sender
// only has to cover the value it moves, not `gas_limit × max_fee`. A real send
// of the same transaction charges gas and must be refused.
//
// This pins the executor's `validate_fees` balance bound (arkiv-reth-executor)
// from the outside: an account funded with exactly the value can dry-run a
// transfer of that value, cannot dry-run one wei more, and cannot send it.

import { beforeAll, describe, expect, test } from "bun:test"
import { createWalletClient, type Hex } from "viem"
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts"
import { connect, type Devnet } from "../src/devnet"

const VALUE = 1_000_000n

let devnet: Devnet
let sender: ReturnType<typeof privateKeyToAccount>
let recipient: Hex

beforeAll(async () => {
  devnet = await connect()
  sender = privateKeyToAccount(generatePrivateKey())
  recipient = privateKeyToAccount(generatePrivateKey()).address

  // Fund the sender with exactly the value it will try to move.
  const hash = await devnet.fundedWallet.sendTransaction({ to: sender.address, value: VALUE })
  const receipt = await devnet.publicClient.waitForTransactionReceipt({ hash })
  expect(receipt.status).toBe("success")
  expect(await devnet.publicClient.getBalance({ address: sender.address })).toBe(VALUE)
})

describe("balance bound with fee charging disabled", () => {
  test("estimateGas accepts a transfer of the whole balance", async () => {
    const gas = await devnet.publicClient.estimateGas({
      account: sender.address,
      to: recipient,
      value: VALUE,
    })
    expect(gas).toBeGreaterThanOrEqual(21_000n)
  })

  test("call accepts a transfer of the whole balance", async () => {
    await expect(
      devnet.publicClient.call({ account: sender.address, to: recipient, value: VALUE }),
    ).resolves.toBeDefined()
  })

  test("estimateGas rejects one wei more than the balance", async () => {
    await expect(
      devnet.publicClient.estimateGas({
        account: sender.address,
        to: recipient,
        value: VALUE + 1n,
      }),
    ).rejects.toThrow(/insufficient funds|lack of funds/i)
  })

  test("call rejects one wei more than the balance", async () => {
    await expect(
      devnet.publicClient.call({ account: sender.address, to: recipient, value: VALUE + 1n }),
    ).rejects.toThrow(/insufficient funds|lack of funds/i)
  })

  test("sending the same transfer is refused once gas is charged", async () => {
    const wallet = createWalletClient({
      chain: devnet.chain,
      transport: devnet.transport,
      account: sender,
    })
    await expect(
      wallet.sendTransaction({ to: recipient, value: VALUE, gas: 21_000n }),
    ).rejects.toThrow(/insufficient funds/i)
    expect(await devnet.publicClient.getBalance({ address: sender.address })).toBe(VALUE)
  })
})
