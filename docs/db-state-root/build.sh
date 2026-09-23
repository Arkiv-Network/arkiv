#!/usr/bin/env bash
# Render the deck in place: diagrams/*.svg and slides.html. Needs python3,
# graphviz and marp-cli on PATH; `nix develop` provides them.
set -euo pipefail
cd "$(dirname "$0")"
python3 gen_diagrams.py diagrams
for f in diagrams/*.dot; do
  dot -Tsvg "$f" -o "${f%.dot}.svg"
done
marp --html slides.md -o slides.html
echo "wrote slides.html"
