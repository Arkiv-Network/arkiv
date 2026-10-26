#!/bin/bash
# Arkiv's wrapper around the stock generator entrypoint (entrypoint-stock.sh).
#
# For `all`, the command ethereum-package runs: make the EL genesis as usual,
# add Arkiv's GolemDB settings to it, and write block 0 as arkiv-reth builds it
# to SHADOW_FORK_FILE, in the eth_getBlockByNumber form the CL genesis tool
# reads. The stock `all` then skips the EL step, since genesis.json exists, and
# builds the CL genesis on that block. Any other command passes through.
set -euo pipefail

if [ "${1:-}" != all ]; then
    exec /work/entrypoint-stock.sh "$@"
fi

# The path comes from the Kurtosis args (ethereum_genesis_generator_params.extra_env).
block_0=$(source /config/values.env && echo "${SHADOW_FORK_FILE:-}")
if [ -z "$block_0" ]; then
    echo "SHADOW_FORK_FILE is not set: the CL genesis would not name arkiv-reth's block 0" >&2
    exit 1
fi

/work/entrypoint-stock.sh el
genesis=/data/metadata/genesis.json

node() { arkiv-reth "$@" --log.stdout.filter error --color never; }

# The dev chain's settings are Arkiv's own, and the node refuses others.
arkiv=$(node dump-genesis --chain dev | sed -n '/^{/,$p' | jq -c '.config.arkiv')
jq --argjson arkiv "$arkiv" '.config.arkiv = $arkiv' "$genesis" > "$genesis.arkiv"
mv "$genesis.arkiv" "$genesis"

datadir=$(mktemp -d)
node init --chain "$genesis" --datadir "$datadir" > /dev/null
out=$(node db --chain "$genesis" --datadir "$datadir" get static-file headers 0)
rm -rf "$datadir"
header=$(sed -n '/^Header$/,/^}$/p' <<< "$out" | sed 1d)
hash=$(sed -n '/^BlockHash$/{n;p}' <<< "$out" | tr -d '"')
jq -n --argjson header "$header" --arg hash "$hash" '{
    jsonrpc: "2.0", id: 1,
    result: ($header + {hash: $hash, transactions: [], uncles: [], withdrawals: []})
}' > "$block_0"
echo "arkiv-reth block 0: $hash"

exec /work/entrypoint-stock.sh all
