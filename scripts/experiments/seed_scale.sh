#!/usr/bin/env bash
# Measure genesis seeding at increasing sizes: how long `arkiv-cli seed-genesis`
# takes and how much memory it peaks at, how big the outputs are, and how long
# `arkiv-reth` takes to initialise a datadir from them — the genesis-file route
# (`node --chain`, the root derived in memory from the alloc) and the JSONL
# route (`init-state`, streamed through reth's ETL importer).
#
# Usage: scripts/experiments/seed_scale.sh [count ...]
#   ARKIV_CLI / ARKIV_RETH   binaries (default: target/debug/…)
#   PAYLOAD_SIZE             bytes per entity (default 1024)
#   SKIP_NODE=1              only time the seeder
#   GENESIS_INIT_MAX=N       skip the genesis-file route above N entities
#                            (`init` holds the parsed alloc in memory, at
#                            roughly 12 KB per 1 KiB entity)
#   WORK                     scratch directory (default: target/tmp/seed-scale;
#                            keep it on disk, /tmp is often a RAM-backed tmpfs)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ARKIV_CLI=${ARKIV_CLI:-$ROOT/target/debug/arkiv-cli}
ARKIV_RETH=${ARKIV_RETH:-$ROOT/target/debug/arkiv-reth}
PAYLOAD_SIZE=${PAYLOAD_SIZE:-1024}
GENESIS_INIT_MAX=${GENESIS_INIT_MAX:-}
WORK=${WORK:-$ROOT/target/tmp/seed-scale}
mkdir -p "$WORK"
COUNTS=${*:-"2000 20000 200000"}

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
# Wall-clock seconds and peak RSS in MiB of a command, on stdout as "secs rss".
# GNU time writes its report to a file of its own: the command's stderr is
# kept apart, for the diagnostics when it fails.
measure() {
    if ! /usr/bin/time -f '%e %M' -o "$WORK/time.out" "$@" >/dev/null 2>"$WORK/stderr.log"; then
        tail -5 "$WORK/stderr.log" >&2
        echo "fail fail"
        return
    fi
    awk '{printf "%s %.0f\n", $1, $2/1024}' < <(tail -1 "$WORK/time.out")
}
size_mb() { du -sm "$1" | cut -f1; }

printf '%-9s %-8s | %-10s %-9s %-9s | %-16s %-7s | %-16s %-7s\n' \
    count payload "seed(s)" "seedRSS" "genesisMB" "init(s)/RSS" "dbMB" "initState(s)/RSS" "db2MB"
for count in $COUNTS; do
    dir="$WORK/$count"; mkdir -p "$dir"
    read -r seed_s seed_rss < <(measure "$ARKIV_CLI" seed-genesis --count "$count" \
        --payload-size "$PAYLOAD_SIZE" --dev-owners 4 --out "$dir/genesis.json")
    genesis_mb=$(size_mb "$dir/genesis.json")

    init_s=-; db_mb=-; init_state_s=-; db2_mb=-
    if [ -z "${SKIP_NODE:-}" ]; then
        # `init` writes the genesis block (hashing the whole alloc) and stops.
        if [ -z "$GENESIS_INIT_MAX" ] || [ "$count" -le "$GENESIS_INIT_MAX" ]; then
            read -r init_s init_rss < <(measure "$ARKIV_RETH" init --chain "$dir/genesis.json" --datadir "$dir/data")
            init_s="$init_s/${init_rss}MiB"
            db_mb=$(size_mb "$dir/data")
        fi

        # The JSONL route: dump + stateHash genesis, then init-state.
        read -r _ _ < <(measure "$ARKIV_CLI" seed-genesis --count "$count" \
            --payload-size "$PAYLOAD_SIZE" --dev-owners 4 --format jsonl --out "$dir/state.jsonl")
        read -r init_state_s init_state_rss < <(measure "$ARKIV_RETH" init-state \
            --chain "$dir/state.jsonl.genesis.json" --datadir "$dir/data2" "$dir/state.jsonl")
        init_state_s="$init_state_s/${init_state_rss}MiB"
        db2_mb=$(size_mb "$dir/data2")
    fi
    printf '%-9s %-8s | %-10s %-9s %-9s | %-16s %-7s | %-16s %-7s\n' \
        "$count" "$PAYLOAD_SIZE" "$seed_s" "${seed_rss}MiB" "$genesis_mb" "$init_s" "$db_mb" "$init_state_s" "$db2_mb"
    rm -rf "$dir/data" "$dir/data2"
done
log "outputs kept under $WORK"
