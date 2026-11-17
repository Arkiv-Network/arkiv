# The single-node dev stack: one auto-sealing `arkiv-reth --dev` on GolemDB,
# with no consensus layer. For the two-node network, see devnet.just.
compose := "docker/compose.yml"

# Builds the arkiv-reth:dev image from source, compiling inside Docker
[group('docker')]
image *args:
  docker buildx build -t arkiv-reth:dev -f docker/arkiv-reth.Dockerfile {{args}} --load .

# The Dockerfile's base image is ubuntu:24.04, the CI runner's distribution. A
# binary built on a newer host needs a newer glibc: set
# ARKIV_PREBUILT_BASE_IMAGE (e.g. ubuntu:26.04), the variable up.py reads too.
#
# Packages an already-built arkiv-reth binary into the arkiv-reth:dev image
[group('docker')]
image_prebuilt binary=(bin_dir / "arkiv-reth"):
  #!/usr/bin/env bash
  set -euo pipefail
  base=()
  if [ -n "${ARKIV_PREBUILT_BASE_IMAGE:-}" ]; then
    base=(--build-arg "BASE_IMAGE=$ARKIV_PREBUILT_BASE_IMAGE")
  fi
  # A context holding only the binary: the image is one COPY of it, and
  # sending a whole target directory to the daemon would mean gigabytes. It
  # lives under target/ rather than /tmp, which is often RAM, and holds a hard
  # link where it can: a debug binary is several GB. The Dockerfile sets the
  # mode itself, so a binary that lost its exec bit (an artifact) is fine.
  mkdir -p target
  context=$(mktemp -d target/image-context.XXXXXX)
  trap 'rm -rf "$context"' EXIT
  ln "{{binary}}" "$context/arkiv-reth" 2>/dev/null || cp "{{binary}}" "$context/arkiv-reth"
  docker build -t arkiv-reth:dev -f docker/arkiv-reth-prebuilt.Dockerfile "${base[@]}" "$context"

# Starts the single-node stack on the arkiv-reth:dev image and waits until it is healthy
[group('docker')]
stack_up:
  docker compose -f {{compose}} up -d --wait
  docker compose -f {{compose}} ps

# Checks the stack's node answers RPC and produces blocks
[group('docker')]
stack_check *args:
  python3 scripts/kurtosis/check.py --no-enclave --sequencer-rpc http://127.0.0.1:8545 {{args}}

# Shows the stack's containers and their logs; e.g. `just stack_logs --tail 100 arkiv-reth`
[group('docker')]
stack_logs *args:
  docker compose -f {{compose}} ps -a
  docker compose -f {{compose}} logs --no-color {{args}}

# Stops the single-node stack and deletes its volumes
[group('docker')]
stack_down:
  docker compose -f {{compose}} down -v
