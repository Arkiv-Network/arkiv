# The Kurtosis enclave the two-node devnet runs in.
enclave := "arkiv-harness"

_bun_frozen := if _ci != "" { "--frozen-lockfile" } else { "" }

# Builds the images from source and brings up the two-node devnet in Kurtosis
[group('kurtosis')]
kurtosis_up name=enclave: image_build genesis_image_build
  python3 scripts/kurtosis/up.py {{name}}

# The binary must suit the image's glibc; see image_prebuilt_build.
#
# Packages an already-built arkiv-reth (see `just build`) and brings up the devnet on it
[group('kurtosis')]
kurtosis_up_prebuilt binary=(bin_dir / "arkiv-reth") name=enclave: (image_prebuilt_build binary) genesis_image_build
  python3 scripts/kurtosis/up.py {{name}}

# Builds the genesis generator image, which takes its arkiv-reth from arkiv-reth:dev
[group('kurtosis')]
genesis_image_build:
  docker build -t arkiv-genesis-generator:dev kurtosis/genesis-generator

# Checks the devnet: RPC, block production, and the follower keeping up
[group('kurtosis')]
kurtosis_check *args:
  python3 scripts/kurtosis/check.py --enclave {{enclave}} {{args}}

# `just kurtosis_logs --lines 1000` starts further back; `--no-follow` prints
# the logs and exits, as CI's diagnostics step does.
#
# Shows the devnet's enclave and its services, then follows their logs
[group('kurtosis')]
[arg('lines', long='lines', help='how many of the latest log lines to show first')]
[arg('follow', long='no-follow', value='false', help='print the logs and exit instead of following them')]
kurtosis_logs lines="200" follow="true" name=enclave:
  -kurtosis enclave ls
  -kurtosis enclave inspect {{name}}
  -kurtosis service logs --all-services --num {{lines}} {{ if follow == "true" { "--follow" } else { "" } }} {{name}}

# Tears the devnet down
[group('kurtosis')]
kurtosis_down name=enclave:
  python3 scripts/kurtosis/down.py {{name}}

# Runs the Arkiv SDK integration tests against the running devnet
[group('kurtosis')]
[working-directory('kurtosis/integration')]
test_kurtosis_sdk *args: _bun_install
  bun test {{args}}

# Restarts the devnet's follower and checks it catches up with the sequencer
[group('kurtosis')]
[working-directory('kurtosis/integration')]
test_kurtosis_sdk_restart: _bun_install
  ARKIV_RESTART_TESTS=1 bun test follower-restart

[working-directory('kurtosis/integration')]
_bun_install:
  bun install {{_bun_frozen}}
