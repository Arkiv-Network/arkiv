# Deferred: testing the tokenomics node with the Arkiv SDK

Status on 2026-10-28: deferred while PR #230 is in review. This note is the context to pick
it up again.

## Goal

Drive a node built from the tokenomics branch through the Arkiv SDK, and check the
tokenomics from outside: the SDK's gas estimates are accepted, `gasUsed` includes storage
gas, the pool at `ARKIV_ADDRESS` grows, and reads return what was written. This is also the
plan's step 8, "Integration check".

## Findings so far

- **The SDK to use is `@arkiv-network/sdk@next`**, recommended by the SDK owner. On
  2026-10-28 `next` was `0.9.0-next.2`, published 2026-10-26; other tags were `latest` 0.8.1,
  `dev` 0.8.1-dev.1 and `advanced` 0.8.0-advanced.3. Pin the exact version: the tag moves.
- **It speaks this node's protocol.** Checked by searching the package, not by running it:
  it sends `execute(...)` to `0x4400…0044`, uses `entityNonce`, `arkiv_query`,
  `arkiv_getEntity` and `arkiv_getBlockTiming`, and ships a `localhost` chain with id 1337,
  the dev chain's id. It exports `createWalletClient`, `createPublicClient`, `createEntity`,
  `extendEntity`, `deleteEntity` and `changeOwnership`. Read its `.d.ts` files for the exact
  signatures before writing tests.
- **SDK 0.7, which `kurtosis/integration` uses today, does not**: it sends compressed RLP to
  a different address, so that project tests no entity writes yet (see its README).
- **No new repository is needed.** `kurtosis/integration` is a bun project with viem, and
  `ARKIV_RPC_URL` / `ARKIV_FUNDED_KEY` let it target a plain dev node.
- **This container has neither bun nor npm.** Install bun
  (`curl -fsSL https://bun.sh/install | bash`) or run the tests on the host.

## Plan

1. **Branch** `matthiaszimmermann/test/sdk-next`, from the tokenomics branch once it is
   rebased on `main`. Open its PR against the tokenomics branch, and retarget it to `main`
   once #230 merges. Fix node bugs the tests find on the tokenomics branch, or in their own
   PR against `main`, not on this branch.
2. **Add the SDK beside 0.7**, under an alias, so the existing tests and the Kurtosis CI job
   are unaffected:

   ```sh
   cd kurtosis/integration
   bun add @arkiv-network/sdk-next@npm:@arkiv-network/sdk@0.9.0-next.2
   ```

   Once 0.9 is released, replace 0.7 with it and drop the alias.
3. **Write `test/sdk-writes.test.ts`**, gated behind `ARKIV_SDK_WRITES=1` until CI runs a
   node built from the tokenomics branch:
   - create, patch, extend, transfer and delete through the SDK, with the SDK's own gas
     estimates;
   - each transaction is mined and succeeds;
   - `gasUsed` includes storage gas, and `eth_getBalance(ARKIV_ADDRESS)` rises by
     `base_fee × storage_gas`;
   - `getEntity` and `select().fetch()` return what was written.
4. **Run it against a local node:**

   ```sh
   cargo build -p arkiv-reth
   ARKIV_STORE_PATH=/tmp/arkiv-golemdb ./target/debug/arkiv-reth node --dev \
     --http --http.api eth,net,web3,txpool
   # in kurtosis/integration
   ARKIV_SDK_WRITES=1 ARKIV_RPC_URL=http://127.0.0.1:8545 \
   ARKIV_FUNDED_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
   bun test sdk-writes
   ```

5. **Update `kurtosis/integration/README.md`**: the new test, the alias, the variable.
6. **Later:** the SDK's own network-health suite against the same node, if its repository has
   one (the plan's step 8), and the same tests on the Kurtosis two-node network on GolemDB.
