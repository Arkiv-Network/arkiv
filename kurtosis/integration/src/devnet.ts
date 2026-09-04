// Clients for the Arkiv devnet that `scripts/kurtosis/up.py` brings up.
//
// Everything is driven through the Arkiv SDK (a viem extension), so the tests
// exercise the same code path an application would. The chain object is built
// at runtime from the RPC's own chain id: ethereum-package picks the id, and
// `arkiv-reth --dev` uses yet another one, so nothing is hard-coded.

import { createPublicClient, createWalletClient } from "@arkiv-network/sdk"
import { defineChain, http, publicActions, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"

// First EL RPC published by `kurtosis/arkiv-chain.yaml` (the sequencer).
export const RPC_URL = process.env.ARKIV_RPC_URL ?? "http://127.0.0.1:32003"

// The first account ethereum-package prefunds in every genesis it generates
// (m/44'/60'/0'/0/0 of its well-known mnemonic). Override with
// ARKIV_FUNDED_KEY; for `arkiv-reth --dev` that is the first hardhat key.
export const FUNDED_KEY = (process.env.ARKIV_FUNDED_KEY ??
  "0xbcdf20249abf0ed6d944c0288fad489e33f66b3960d9e6229c1cd214ed3bbe31") as Hex

export async function connect() {
  const transport = http(RPC_URL)
  const probe = createPublicClient({ transport })
  const id = await probe.getChainId()

  const chain = defineChain({
    id,
    name: `arkiv-devnet-${id}`,
    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: [RPC_URL] } },
  })

  // SDK 0.7 types only a subset of viem's public actions; the full set is
  // there at runtime, and `call` / `estimateGas` are what the fee tests need.
  const publicClient = createPublicClient({ chain, transport }).extend(publicActions)
  const funded = privateKeyToAccount(FUNDED_KEY)
  const fundedWallet = createWalletClient({ chain, transport, account: funded })

  return { chain, transport, publicClient, funded, fundedWallet }
}

export type Devnet = Awaited<ReturnType<typeof connect>>
