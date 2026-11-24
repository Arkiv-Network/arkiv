#!/usr/bin/env bash
# Prefill the committed fixture with a built arkiv-reth and check that block 0
# comes out as the fixture's expected.json says.
#
#   scripts/prefill/check.sh [path/to/arkiv-reth]           # check
#   scripts/prefill/check.sh [path/to/arkiv-reth] --update  # rewrite expected.json
#
# Block 0 depends only on the fixture's files, so a mismatch means the prefill
# now writes different state (GolemDB's encoding, the executor's writes, the
# archive format). If that is intended, rerun with --update and commit the file.
set -euo pipefail

bin=${1:-target/debug/arkiv-reth}
fixture=crates/arkiv-prefill/fixtures/small

fail() {
  echo "error: $*" >&2
  exit 1
}

prefill() {
  "$bin" prefill --chain "$work/genesis.json" --archives "$work/set.json" --store "$work/golemdb" "$@"
}

[ -x "$bin" ] || fail "no arkiv-reth at $bin; build it with 'just build'"

# Work on a copy: the finalize rewrites the genesis file.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -R "$fixture/." "$work/"

echo "== prefill"
prefill

echo "== prefill again: nothing left to write"
prefill | tee "$work/again.log"
! grep -q "committed as commit" "$work/again.log" || fail "the second run wrote an archive again"

echo "== finalize"
prefill --finalize | tee "$work/finalize.log"

echo "== finalize again: must be refused"
! prefill --finalize || fail "a second finalize was accepted"

# What this run produced, in expected.json's shape.
root=$(jq -r .config.arkiv.prefilledBlockZeroStateRoot "$work/genesis.json")
hash=$(grep -o 'hash 0x[0-9a-f]*' "$work/finalize.log" | cut -d' ' -f2)
actual=$(jq -n --arg root "$root" --arg hash "$hash" '{root: $root, blockZeroHash: $hash}')

if [ "${2:-}" = --update ]; then
  echo "$actual" >"$fixture/expected.json"
  echo "wrote $fixture/expected.json"
  exit 0
fi

diff <(jq . "$fixture/expected.json") <(echo "$actual") ||
  fail "block 0 differs from $fixture/expected.json; if intended, rerun with --update and commit it"
echo "ok: block 0 matches $fixture/expected.json"
