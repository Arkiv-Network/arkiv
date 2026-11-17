# The GolemDB demo: a local `arkiv-reth --dev` on host binaries, no Kurtosis.

# Builds the binaries, then runs the GolemDB demo on them (see scripts/demo/README.md)
[group('demo')]
demo *args: build
  ARKIV_RETH_BINARY="${ARKIV_RETH_BINARY:-{{justfile_directory() / bin_dir / "arkiv-reth"}}}" \
  ARKIV_CLI_BINARY="${ARKIV_CLI_BINARY:-{{justfile_directory() / bin_dir / "arkiv-cli"}}}" \
    python3 scripts/demo/run.py {{args}}
