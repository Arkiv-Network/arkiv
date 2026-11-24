#!/usr/bin/env bash
# Prefill the committed fixture with a built arkiv-reth, finalize it, and check
# block 0 against the root and hash the fixture expects.
#
#   scripts/prefill/check.sh [path/to/arkiv-reth]           # check
#   scripts/prefill/check.sh [path/to/arkiv-reth] --update  # rewrite expected.json
#
# The fixture's block 0 is fixed by its genesis file and archives alone, so any
# change to it means the prefill no longer writes the same state: a change to
# GolemDB's encoding, the executor's writes, or the archive format. When that is
# intended, rerun with --update and commit the new expected.json.
#
# It also checks that a second run writes nothing and a second finalize is
# refused, so the resume and the finalize guards are exercised by the binary
# itself, not only by unit tests.
set -euo pipefail

bin=${1:-target/debug/arkiv-reth}
update=${2:-}
fixture=crates/arkiv-prefill/fixtures/small

if [ ! -x "$bin" ]; then
  echo "error: no arkiv-reth at $bin; build it with \`just build\`" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -R "$fixture/." "$work/"
rm -f "$work/expected.json"
prefill() {
  "$bin" prefill --chain "$work/genesis.json" --archives "$work/set.json" --store "$work/golemdb" "$@"
}

echo "== prefill"
prefill
echo "== prefill again: every archive is already in"
again=$(prefill)
echo "$again"
if grep -q "committed as commit" <<<"$again"; then
  echo "error: the second run wrote an archive again" >&2
  exit 1
fi

echo "== finalize"
out=$(prefill --finalize)
echo "$out"
root=$(jq -r '.config.arkiv.prefilledBlockZeroStateRoot' "$work/genesis.json")
hash=$(sed -n 's/^block 0 committed as commit [0-9]*: state root 0x[0-9a-f]*, hash \(0x[0-9a-f]*\)$/\1/p' <<<"$out")
if [ -z "$root" ] || [ "$root" = null ] || [ -z "$hash" ]; then
  echo "error: finalize did not report block 0's root and hash" >&2
  exit 1
fi

echo "== finalize again: refused"
if prefill --finalize; then
  echo "error: a second finalize was accepted" >&2
  exit 1
fi

if [ "$update" = --update ]; then
  jq -n --arg root "$root" --arg hash "$hash" '{root: $root, blockZeroHash: $hash}' >"$fixture/expected.json"
  echo "wrote $fixture/expected.json: root $root, block 0 hash $hash"
  exit 0
fi

expected_root=$(jq -r .root "$fixture/expected.json")
expected_hash=$(jq -r .blockZeroHash "$fixture/expected.json")
status=0
if [ "$root" != "$expected_root" ]; then
  echo "error: block 0's root is $root, the fixture expects $expected_root" >&2
  status=1
fi
if [ "$hash" != "$expected_hash" ]; then
  echo "error: block 0's hash is $hash, the fixture expects $expected_hash" >&2
  status=1
fi
if [ "$status" -ne 0 ]; then
  echo "If the change is intended, rerun with --update and commit $fixture/expected.json" >&2
  exit 1
fi
echo "ok: block 0 root $root, hash $hash"
