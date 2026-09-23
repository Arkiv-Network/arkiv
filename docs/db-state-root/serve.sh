#!/usr/bin/env bash
# Serve the deck with live reload: marp's server mode re-renders slides.md on
# every save and reloads the browser; a watcher regenerates the diagrams when
# gen_diagrams.py changes. Needs the flake's dev shell: `nix develop -c ./serve.sh`.
# The deck is at http://localhost:${PORT:-8080}/slides.md
set -euo pipefail
cd "$(dirname "$0")"

render_diagrams() {
  python3 gen_diagrams.py diagrams
  for f in diagrams/*.dot; do
    dot -Tsvg "$f" -o "${f%.dot}.svg"
  done
}
export -f render_diagrams

render_diagrams
watchexec --quiet --watch gen_diagrams.py -- bash -c 'render_diagrams && touch slides.md' &
trap 'kill $! 2>/dev/null' EXIT

PORT="${PORT:-8080}" exec marp --server --html .
