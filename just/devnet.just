# The Kurtosis enclave the two-node devnet runs in.
enclave := "arkiv-harness"

_bun_frozen := if _ci != "" { "--frozen-lockfile" } else { "" }

# Set ARKIV_RETH_BINARY to package an already-built binary instead of
# compiling the workspace inside Docker.
#
# Builds the arkiv-reth image and brings up the two-node devnet in Kurtosis
[group('devnet')]
devnet_up name=enclave:
  python3 scripts/kurtosis/up.py {{name}}

# Checks the devnet: RPC, block production, and the follower keeping up
[group('devnet')]
devnet_check *args:
  python3 scripts/kurtosis/check.py --enclave {{enclave}} {{args}}

# Shows the devnet's enclave, its services and their recent logs
[group('devnet')]
devnet_logs name=enclave:
  -kurtosis enclave ls
  -kurtosis enclave inspect {{name}}
  -kurtosis service logs --all-services --num 200 {{name}}

# Tears the devnet down
[group('devnet')]
devnet_down name=enclave:
  python3 scripts/kurtosis/down.py {{name}}

# Runs the Arkiv SDK integration tests against the running devnet
[group('devnet')]
[working-directory('kurtosis/integration')]
sdk_test *args: _bun_install
  bun test {{args}}

# Restarts the devnet's follower and checks it catches up with the sequencer
[group('devnet')]
[working-directory('kurtosis/integration')]
sdk_restart_test: _bun_install
  ARKIV_RESTART_TESTS=1 bun test follower-restart

[working-directory('kurtosis/integration')]
_bun_install:
  bun install {{_bun_frozen}}

# Builds the binaries, then runs the GolemDB demo on them (see scripts/demo/README.md)
[group('devnet')]
demo *args: build
  ARKIV_RETH_BINARY="${ARKIV_RETH_BINARY:-{{justfile_directory() / bin_dir / "arkiv-reth"}}}" \
  ARKIV_CLI_BINARY="${ARKIV_CLI_BINARY:-{{justfile_directory() / bin_dir / "arkiv-cli"}}}" \
    python3 scripts/demo/run.py {{args}}
