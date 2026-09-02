# Block base fee in arkiv-reth

How the EIP-1559 base fee is computed on an Arkiv chain, which parameters feed it,
where each parameter is set per environment, and how the no-EVM executor
interacts (and does not interact) with it.

Source references are to reth `v2.5.0` (the tag pinned in `Cargo.toml`) and
`alloy-eips 2.4.1` unless a path starts with `crates/` or `bin/`, which is this
repository.

## TL;DR

- Arkiv adds **one rule** to EIP-1559: the base fee never drops below the
  genesis block's base fee. It lives in `crates/arkiv-reth-chainspec`
  (`ArkivChainSpec`), see §7. Everything else about the base fee is reth's:
  the formula, consensus validation, the payload builder and the tx pool.
- The floor is set by the **genesis** `baseFeePerGas` (1 gwei when absent,
  which is also what `--dev` gets). A genesis with `"baseFeePerGas": "0xa"`
  floors the chain at 10 wei. There is no separate knob.
- The formula above the floor is plain EIP-1559 with Ethereum mainnet
  parameters: gas target is half the gas limit, and the base fee moves by at
  most 1/8 per block. There is no genesis JSON field that changes those two
  constants.
- The other tunables are the genesis `gasLimit`, the builder's desired gas
  limit (`--builder.gaslimit`), and node-local **tx-pool** floors
  (`--txpool.minimal-protocol-fee`, `--txpool.minimum-priority-fee`). None of
  the deployed environments set the tx-pool floors today.
- Without the rule (releases up to `v0.1.0`) the protocol floor is **7 wei**:
  an idle chain decays there in about 140 blocks.
- The Arkiv executor settles fees as revm does: the sender pays
  `gas_used × min(max_fee, base_fee + tip)`, the base-fee share is burned, the
  tip goes to the block beneficiary, and the fee cap, priority fee and sender
  balance are validated first (§6). Up to `v0.1.0` it charged the fee cap,
  burned all of it, and validated none of that.

## 1. The formula

`EthChainSpec::next_block_base_fee` (`crates/chainspec/src/api.rs:67`) is the
single derivation point. This section describes reth's stock derivation; §7
describes the Arkiv floor applied on top of it.

```text
next_base_fee = calc_next_block_base_fee(
    parent.gas_used,
    parent.gas_limit,
    parent.base_fee_per_gas,
    chain_spec.base_fee_params_at_timestamp(next_timestamp),
)
```

`calc_next_block_base_fee` (`alloy-eips/src/eip1559/helpers.rs:92`):

```text
gas_target = gas_limit / elasticity_multiplier          // 2 → target is half the limit

if gas_used == gas_target:  base_fee
if gas_used >  gas_target:  base_fee + max(1, base_fee * (gas_used - gas_target) / (gas_target * denominator))
if gas_used <  gas_target:  base_fee - base_fee * (gas_target - gas_used) / (gas_target * denominator)
```

With `BaseFeeParams::ethereum()` (`max_change_denominator = 8`,
`elasticity_multiplier = 2`, `alloy-eips/src/eip1559/constants.rs:30-33`):

| Situation | Effect on next base fee |
|---|---|
| Block full (`gas_used == gas_limit`) | +12.5 % |
| Block at target (`gas_used == gas_limit / 2`) | unchanged |
| Block empty | −12.5 % (integer division, floors at 7 wei) |

The decrease term is `base_fee / 8` for an empty block, which is zero once
`base_fee < 8`. That is why `MIN_PROTOCOL_BASE_FEE = 7`
(`constants.rs:21`) is the effective floor of stock reth. It is a consequence
of the arithmetic, not a configured minimum. From the 1 gwei genesis value an
idle chain reaches 7 wei after roughly 140 empty blocks, i.e. about 35 s on the
250 ms dev chain, or about 5 minutes on a 2 s slot chain. On Arkiv the floor
from §7 stops the decay at the genesis value instead.

### Genesis block

`make_genesis_header` (`crates/chainspec/src/spec.rs:46`): if London is active
at block 0, the genesis header's base fee is `genesis.baseFeePerGas` when
present, else `INITIAL_BASE_FEE = 1_000_000_000` (1 gwei). Block 1 is then
derived from block 0 with the formula above; there is no second special case.

### Base fee parameters cannot come from genesis JSON

`ChainSpec.base_fee_params` is `BaseFeeParamsKind::Constant(BaseFeeParams::ethereum())`
for every chain reth knows, including `DEV` (`spec.rs:256`) and anything loaded
with `--chain <genesis.json>`: `impl From<Genesis> for ChainSpec`
(`spec.rs:934-946`) fills the field from `..Default::default()` and reads no
genesis key for it. `BaseFeeParamsKind::Variable` (per-hardfork parameters)
exists only for chains built in Rust, e.g. OP-stack.

## 2. Who consumes the value

All four consumers call the same `next_block_base_fee`, so they agree by
construction. Changing the derivation in one place without the others forks the
chain.

| Consumer | Where | What it does |
|---|---|---|
| Payload builder (block producer) | `crates/ethereum/evm/src/lib.rs:232` (`next_evm_env`) | Puts the derived fee in `BlockEnv.basefee`; `build.rs:79` copies it into the sealed header. |
| Consensus (every importing node) | `crates/consensus/common/src/validation.rs:321` (`validate_against_parent_eip1559_base_fee`) | Recomputes it from the parent and rejects the block with `BaseFeeDiff` if the header disagrees. This makes the base fee a **consensus rule**. |
| Transaction pool | `crates/transaction-pool/src/maintain.rs:152,339,445` → `pool/txpool.rs:283` (`update_basefee`) | On every new canonical tip, recomputes the *pending* base fee and moves transactions between the `Pending` and `BaseFee` sub-pools. Only `Pending` transactions (fee cap ≥ pending base fee) are offered to the builder. |
| RPC | `crates/rpc/rpc-eth-api/src/helpers/fee.rs:389` | `eth_gasPrice = latest_header.base_fee + suggested_tip`; `eth_feeHistory` reports per-block base fees and the derived next one; receipts carry `effectiveGasPrice = min(max_fee, base_fee + max_priority_fee)` (`rpc-eth-types/src/receipt.rs:46`). |

## 3. The gas limit input

The base fee reacts to `gas_used` relative to `gas_limit / 2`, so the block gas
limit is the other half of the picture. The producer chooses it per block in
`crates/ethereum/payload/src/lib.rs:200`:

```text
gas_limit = clamp(desired, parent.gas_limit ∓ (parent.gas_limit / 1024 - 1))
```

`desired` is, in order (`crates/node/core/src/cli/config.rs:39`):

1. the CL's payload attribute `target_gas_limit` if it sends one (post-Gloas only, so not today);
2. `--builder.gaslimit` if given;
3. otherwise **36,000,000** for any chain that is not Ethereum mainnet/Sepolia/Holesky/Hoodi (those get 60 M).

So a chain whose genesis `gasLimit` differs from the builder's desired value
drifts toward the desired value by about 0.1 % per block, and the gas target
(and thus the base-fee equilibrium) drifts with it. See the per-environment
notes below: this applies to both tiramisu and the dev chain.

Gas *used* on Arkiv is what the executor reports: a flat 21,000 for value
transfers and view calls, and the metered op cost for entity batches, both
floored at the EIP-7623 intrinsic gas of the calldata
(`crates/arkiv-reth-executor/src/lib.rs:123-143`).

## 4. Tunables and defaults

### Genesis JSON (`--chain <file>`)

| Key | Meaning | Default when absent |
|---|---|---|
| `baseFeePerGas` | Base fee of block 0, **and the Arkiv floor for every later block** (§7) | 1 gwei (`INITIAL_BASE_FEE`) |
| `gasLimit` | Gas limit of block 0; later blocks drift toward the builder's desired value | required |
| `config.londonBlock` | Block from which EIP-1559 applies; must be 0 for the above to hold from genesis | — |

`arkiv-cli inject-predeploy` (`bin/arkiv-cli/src/main.rs:847`) only adds dev
funding to `alloc`; it does not touch any of these.

### `arkiv-reth node` flags (all stock reth)

| Flag | Default | Scope | Effect |
|---|---|---|---|
| `--builder.gaslimit` | 36 M (non-named chain) | producer only | Desired block gas limit; see §3. |
| `--txpool.minimal-protocol-fee` | 7 | node-local | Reject a tx whose `max_fee_per_gas` is below this at pool insert (`pool/txpool.rs:2027`, `FeeCapBelowMinimumProtocolFeeCap`). Does **not** change the block base fee; a block containing such a tx from another producer is still valid. |
| `--txpool.minimum-priority-fee` | none | node-local | Reject non-local EIP-1559 txs whose tip is below this (`validate/eth.rs:578`). |
| `--txpool.pricebump` | 10 % | node-local | Replacement threshold. |
| `--txpool.gas-limit` | 30 M | node-local | Per-tx gas limit cap enforced by the pool. |
| `--gpo.blocks` / `--gpo.percentile` / `--gpo.maxprice` / `--gpo.ignoreprice` / `--gpo.default-suggested-fee` | 20 / 60 / 500 gwei / 0 / 1 gwei | RPC only | Shape `eth_gasPrice`'s suggested tip (`rpc-eth-types/src/gas_oracle.rs:135`). With no populated blocks the suggestion is the default 1 gwei, so `eth_gasPrice` on an idle chain returns about 1 gwei + base fee. |
| `--dev` / `--dev.block-time` | off / on-demand | dev only | Local auto-sealer. Blocks are built at the interval regardless of traffic, so the base fee decays continuously while idle. |

There is no flag for the base-fee parameters and no flag that makes the base
fee constant. The minimum base fee is not a flag either: it is the genesis
`baseFeePerGas`, so that all nodes of a chain agree on it by construction.

## 5. Per-environment settings

| Environment | Genesis base fee | Genesis gas limit | Builder desired gas limit | Pool floors | Block time |
|---|---|---|---|---|---|
| **Dev** (`arkiv-reth node --dev`; harness `crates/arkiv-harness/src/node.rs`; `docker/arkiv-reth-dev.Dockerfile`) | 1 gwei (reth `dev.json` has none) | 30 M | 36 M (default, chain id 1337 is not a named chain) | defaults | `--dev.block-time 250ms` |
| **Kurtosis** (`kurtosis/arkiv-chain.yaml`) | ethereum-package generated | 60 M (`genesis_gaslimit` default) | 36 M (package's `gas_limit` default is 0, so no `--builder.gaslimit` is passed) | defaults | 2 s slots |
| **Tiramisu testnet** (`db-chain-networks/networks/tiramisu/genesis.json`, `db-chain-mgr` chart `producer.yaml`/`watchers.yaml`) | 1 gwei (`baseFeePerGas` null, `londonBlock` 0) | 60 M | 36 M (chart passes no `--builder.gaslimit`) | defaults (chart passes no `--txpool.*` fee flags) | CL slot time |
| **arkiv-tests PoS devnet** (`arkiv-tests/prod/run-el.sh`) | per its genesis | per its genesis | `BLOCK_GAS_LIMIT` env, default 30 M | `TXPOOL_MIN_PROTOCOL_FEE` / `TXPOOL_MIN_PRIORITY_FEE` env, unset by default | CL slot time |

Two consequences worth knowing:

- **Tiramisu's block gas limit is 36 M, not 60 M.** The producer drifts the
  limit from the 60 M genesis toward reth's 36 M default in roughly 520 blocks,
  and the gas target is therefore 18 M. To keep 60 M the producers need
  `--builder.gaslimit 60000000`. The dev chain does the opposite, growing from
  30 M toward 36 M.
- **On `v0.1.0`, the base fee of every deployed Arkiv chain sits at 7 wei when
  traffic is below target.** Clients that pin a price (the harness's
  `EXECUTE_GAS_PRICE` and the e2e probe both pin 1 gwei) massively overpay, and
  on `v0.1.0` that overpayment is charged in full (§6). With the §7 rule the
  same chains sit at their genesis base fee, 1 gwei, so a 1 gwei client pays
  the going rate.

## 6. What the Arkiv executor does with fees

`ArkivEvm::transact_raw` (`crates/arkiv-reth-executor/src/lib.rs`) bypasses
revm's whole handler pipeline, so its pre-execution validation and its
post-execution fee settlement are re-done in the executor. As of this branch
they match revm:

- **Validation** (`validate_fees`), before anything is charged, as typed
  invalid-transaction errors so the payload builder skips the transaction
  rather than aborting the block: the priority fee may not exceed the fee cap;
  the effective price may not fall below the block base fee; the sender must
  cover `gas_limit × max_fee + value`, or just the value when gas is not going
  to be charged. Each honours the revm `CfgEnv` flag reth sets for the matching
  path (`eth_call` and `eth_estimateGas` disable the base-fee check and fee
  charging; engine-tree payload prewarming, whose results are cache-only,
  disables the balance check along with the nonce and base-fee checks).
- **Settlement** (`charge_sender`): the sender pays `gas_used × effective`,
  where `effective = min(max_fee, base_fee + priority_fee)` for EIP-1559 and
  the `gasPrice` for legacy transactions. `gas_used × base_fee` is burned and
  `gas_used × (effective − base_fee)` is credited to the block beneficiary,
  the CL's `--suggested-fee-recipient` (`feeRecipient` in the chart). A zero
  tip credits nothing, so the beneficiary is never touched into existence as
  an empty account. Only gas used is charged, never the limit.
- **Receipts agree with state.** reth's RPC computes `effectiveGasPrice` the
  same way, so the receipt's figure times gas used is exactly the balance
  change.

Up to `v0.1.0` none of this held: all three paths charged
`gas_used × tx.gas_price`, which for EIP-1559 transactions is the fee cap, with
no refund of `max_fee − effective`; nothing was credited to the beneficiary;
the fee cap was never checked against the base fee; and the balance debit
saturated, so an underfunded sender paid what it had and the transaction still
succeeded.

The header base fee itself is untouched by all of this: it is set and validated
by reth exactly as on Ethereum, and block validity depends on it.

## 7. The Arkiv rule: a minimum base fee equal to the genesis base fee

Implemented in `crates/arkiv-reth-chainspec`:

```text
next_base_fee = max(eip1559(parent), genesis.baseFeePerGas)
```

`ArkivChainSpec` wraps reth's `ChainSpec`, delegates every trait method to it,
and overrides `EthChainSpec::next_block_base_fee` with the clamp. The floor is
read once from the genesis header (`ChainSpec::initial_base_fee`), so it is
`baseFeePerGas` from the genesis JSON, or 1 gwei when the field is absent. If
London is not active at genesis the header has no base fee and the rule is
inert.

Because every consumer in §2 reaches the derivation through that one trait
method (there are no direct calls to alloy's `calc_next_block_base_fee` in
reth v2.5.0 outside it), the builder, consensus, the pool and RPC all apply the
same floor. `eth_gasPrice` and `eth_feeHistory` report the floored value, the
pool parks transactions whose fee cap is below it, and consensus rejects any
header that does not carry it.

### What it took to plug in

reth's `EthereumNode`, its Ethereum CLI `run`, and the previous
`ArkivExecutorBuilder` bound were all hard-wired to reth's concrete
`ChainSpec`, so the node now has its own types:

- `arkiv-reth-chainspec`: `ArkivChainSpec` and `ArkivChainSpecParser`, the
  `--chain` value parser. Named chains are reduced to `dev`; a genesis file or
  inline genesis JSON is parsed as before. This follows reth's own
  `examples/custom-hardforks` pattern.
- `bin/arkiv-reth/src/node.rs`: `ArkivNode`, the Ethereum node types on
  `ArkivChainSpec` with the Arkiv executor, reusing reth's pool, network,
  payload, consensus and RPC add-ons.
- `bin/arkiv-reth/src/main.rs`: `Cli::<ArkivChainSpecParser>` and
  `run_with_components::<ArkivNode>` in place of `run`, which is bound to reth's
  `ChainSpec`.
- `ArkivExecutorBuilder` is generic over the chain spec, with the bounds reth's
  own executor builder uses.

### Operational notes

- This is a **consensus change**. A `v0.1.0` node rejects every block a node
  with this rule produces once the derived fee would fall below the floor
  (`BaseFeeDiff`), and vice versa. A chain has to be started on the rule, or
  every producer and watcher switched together at a known block.
- The tx pool's `--txpool.minimal-protocol-fee` (default 7 wei) is independent
  of the floor. A transaction priced between 7 wei and the floor is accepted
  into the pool and parked, never mined. Setting that flag to the genesis base
  fee makes such transactions fail at submission instead, which is friendlier
  to clients.
- With the floor in place, the pool only offers transactions whose fee cap is
  at least the floor, so every mined sender pays at least the floor per gas.

### Tests

- `crates/arkiv-reth-chainspec`: unit tests pin the floor to the genesis value
  (explicit, absent, `--dev`), show an empty block holding the floor where
  stock reth decays, show plain EIP-1559 above the floor, and the no-London
  case.
- `bin/arkiv-reth/tests/e2e.rs`: an idle `--dev` node reports 1 gwei on every
  block, and a node on a genesis file with `baseFeePerGas` of 10 wei reports 10
  wei on every block.
