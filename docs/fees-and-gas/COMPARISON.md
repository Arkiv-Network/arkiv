# Base fee: before and after the minimum-base-fee rule

Quick reference for what changes when `scx1332/min-base-fee` lands. Details and
source references are in [base-fee.md](base-fee.md).

| | Before (`v0.1.0`, `main` today) | After merge |
|---|---|---|
| Base fee formula | Stock EIP-1559 (reth) | Stock EIP-1559, then clamped to a floor |
| Minimum base fee | 7 wei (arithmetic floor of the formula) | Genesis `baseFeePerGas` (1 gwei when absent) |
| Idle chain settles at | 7 wei after ~140 empty blocks | The genesis base fee, from block 1 on |
| Behaviour under load | +12.5 % per full block, −12.5 % per empty block | Same above the floor; decays back to the floor, not below |
| Where the floor is set | Nowhere | `baseFeePerGas` in the genesis JSON; no flag, no constant |
| Applies to | — | Payload builder, consensus validation, tx pool, `eth_gasPrice` / `eth_feeHistory` (one shared derivation) |
| Chain-spec type | reth `ChainSpec` via `EthereumNode` | `ArkivChainSpec` (`crates/arkiv-reth-chainspec`) via `ArkivNode` |
| `--chain` named values | `mainnet`, `sepolia`, `holesky`, `hoodi`, `dev` | `dev` only; genesis file or inline JSON as before |
| `--dev` floor | none (decays to 7 wei in ~35 s at 250 ms blocks) | 1 gwei (reth's dev genesis has no `baseFeePerGas`) |
| Tiramisu (1 gwei genesis) | Sits at 7 wei when below target | Would sit at 1 gwei; needs a new network, old nodes reject the blocks |
| Consensus compatibility | — | Breaking: old and new nodes diverge once the fee would drop below the floor |
| Tx pool minimum (`--txpool.minimal-protocol-fee`) | 7 wei default, unchanged | 7 wei default, unchanged; set it to the genesis base fee so under-priced txs fail at submission instead of parking |
| What a sender pays (executor) | `gas_used × max_fee_per_gas`, burned | Unchanged; but the pool now only offers txs with fee cap ≥ floor, so every sender pays at least the floor per gas |
| Block gas limit | Drifts to 36 M unless `--builder.gaslimit` is set | Unchanged |
