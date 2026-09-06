#!/usr/bin/env bash
# Time `arkiv-reth init-state` phase by phase at increasing seed sizes, from
# the progress file the command keeps, and record its heap.
#
# For each count: `arkiv-cli seed-genesis --format jsonl` streams a dump, then
# `arkiv-reth init-state` imports it while this script samples
# `<datadir>/init-state-progress.json` and the importer's RssAnon once a
# second. One line per count: seed time, the parse / write / hash phases of
# the import, its total, its peak heap (RssAnon: reth's MDBX map is page
# cache and would inflate RSS), and the datadir size.
#
# Usage: scripts/experiments/init_state_phases.sh [count ...]
#   ARKIV_CLI / ARKIV_RETH   binaries (default: target/release-fast/…)
#   PAYLOAD_SIZE             bytes per entity (default 1024)
#   KEEP=1                   keep each count's dump and datadir
#   WORK                     scratch directory (default: target/tmp/init-state-phases;
#                            keep it on disk, /tmp is often a RAM-backed tmpfs)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ARKIV_CLI=${ARKIV_CLI:-$ROOT/target/release-fast/arkiv-cli}
ARKIV_RETH=${ARKIV_RETH:-$ROOT/target/release-fast/arkiv-reth}
PAYLOAD_SIZE=${PAYLOAD_SIZE:-1024}
WORK=${WORK:-$ROOT/target/tmp/init-state-phases}
mkdir -p "$WORK"
COUNTS=${*:-"200000 500000 1000000"}

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
now() { date +%s.%N; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.1f", b - a }'; }

# The phase named in a progress file, or "-" while it is not there yet.
phase_of() { python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1]))["phase"])
except Exception: print("-")' "$1"; }

printf '%-9s %-8s | %-8s | %-8s %-8s %-8s %-8s | %-9s %-8s\n' \
    count payload "seed(s)" "parse(s)" "write(s)" "hash(s)" "total(s)" "peakMiB" "dbMB"
for count in $COUNTS; do
    dir="$WORK/$count"; rm -rf "$dir"; mkdir -p "$dir"
    dump="$dir/state.jsonl"

    log "seeding $count"
    t0=$(now)
    "$ARKIV_CLI" seed-genesis --count "$count" --payload-size "$PAYLOAD_SIZE" \
        --dev-owners 4 --format jsonl --out "$dump" \
        --progress-file "$dir/seed-progress.json" >/dev/null 2>"$dir/seed.stderr"
    seed_s=$(secs "$t0" "$(now)")

    log "importing $count"
    progress="$dir/data/init-state-progress.json"
    t0=$(now)
    "$ARKIV_RETH" init-state --chain "$dump.genesis.json" --datadir "$dir/data" "$dump" \
        >/dev/null 2>"$dir/init-state.stderr" &
    pid=$!
    # Phase transitions as they show in the progress file, and the peak heap.
    last="-"; peak=0; declare -A at=()
    while kill -0 "$pid" 2>/dev/null; do
        phase=$(phase_of "$progress")
        if [ "$phase" != "$last" ]; then
            at[$phase]=$(now); last=$phase
        fi
        anon=$(awk '/RssAnon/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
        (( anon > peak )) && peak=$anon
        sleep 1
    done
    if ! wait "$pid"; then
        tail -3 "$dir/init-state.stderr" >&2
        log "init-state failed at $count"
        continue
    fi
    t1=$(now)
    at[done]=$t1
    parse_s=$(secs "${at[parsing]:-$t0}" "${at[writing]:-$t1}")
    write_s=$(secs "${at[writing]:-$t1}" "${at[hashing]:-$t1}")
    hash_s=$(secs "${at[hashing]:-$t1}" "$t1")
    total_s=$(secs "$t0" "$t1")
    db_mb=$(du -sm "$dir/data" | cut -f1)

    printf '%-9s %-8s | %-8s | %-8s %-8s %-8s %-8s | %-9s %-8s\n' \
        "$count" "$PAYLOAD_SIZE" "$seed_s" "$parse_s" "$write_s" "$hash_s" "$total_s" \
        "$((peak / 1024))" "$db_mb"
    [ -n "${KEEP:-}" ] || rm -rf "$dir/data" "$dump"
done
