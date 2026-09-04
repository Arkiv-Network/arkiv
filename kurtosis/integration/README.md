# Kurtosis integration tests

Black-box tests that drive the Arkiv devnet through the
[Arkiv SDK](https://www.npmjs.com/package/@arkiv-network/sdk), run with
[bun](https://bun.sh). They expect a chain that is already up: in CI the
`kurtosis.yml` workflow runs them after `scripts/kurtosis/up.py` has brought the
two-node enclave up and the health check has passed.

```sh
scripts/kurtosis/up.py            # once; builds the image and starts the enclave
cd kurtosis/integration
bun install
bun test
```

| Variable           | Default                                  | Meaning                                        |
| ------------------ | ---------------------------------------- | ---------------------------------------------- |
| `ARKIV_RPC_URL`    | `http://127.0.0.1:32003`                 | EL RPC to test; the sequencer's published port |
| `ARKIV_FUNDED_KEY` | ethereum-package's first prefunded account | Pays for the transfers the tests need         |

The chain id is read from the RPC, so the same tests run against a bare
`arkiv-reth node --dev`:

```sh
ARKIV_RPC_URL=http://127.0.0.1:8545 \
ARKIV_FUNDED_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
bun test
```

## What is covered

- `test/fee-charge.test.ts` — the executor's pre-execution balance bound when
  fee charging is disabled. `eth_call` and `eth_estimateGas` must accept a
  transfer of the sender's whole balance, refuse one wei more, and a real send
  of that transfer must be refused because gas is charged there.
- `test/sdk-reads.test.ts` — the `arkiv_*` reads whose wire shape this node and
  SDK 0.7 agree on: `arkiv_getBlockTiming` and `arkiv_getEntityCount`.

The SDK's entity writes (`createEntity` and friends) and `select().fetch()`
are not exercised: SDK 0.7 sends brotli-compressed RLP to
`0x…61726b6976` and `arkiv_query` options named `includeData` /
`resultsPerPage`, while this node expects ABI-encoded `execute(Operation[])`
calls to `0x44…0044` and `select` / `limit`. Add those tests once the two
agree.
