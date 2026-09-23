# Arkiv under its own state root

A deck on PR #122: the Arkiv database moved out of reth's accounts into
persistent Merkle-Patricia tries committed by one root in an anchor slot.

- `slides.md`: the deck, Marp markdown. Readable as plain Markdown too.
- `gen_diagrams.py`: emits the diagrams as Graphviz DOT.
- `flake.nix`: pins the toolchain (python, graphviz, marp-cli).

Build:

```sh
nix build                   # result/slides.html and result/diagrams/*.svg
nix develop -c ./build.sh   # or in place
nix develop -c ./serve.sh   # live: http://localhost:8080/slides.md reloads on save
```

`slides.html` is self-contained apart from the SVGs next to it.
