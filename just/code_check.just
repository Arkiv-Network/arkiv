# The black release the Python helpers are formatted with.
black_version := "24.10.0"

# The nightly cargo-udeps runs on: it needs a nightly rustc.
udeps_toolchain := "nightly-2026-10-03"

# Formats the Rust code; `just fmt --check` only checks it
[group('code_check')]
fmt *args:
  cargo fmt --all {{args}}


# Runs clippy on the workspace crates
[group('code_check')]
clippy *args:
  cargo clippy --all-targets {{locked}} {{_cargo_profile}} --no-deps {{args}} -- -D warnings

# Checks the Rust formatting, then runs clippy. It fails if any of the checks fail
[group('code_check')]
check_code: (fmt "--check") clippy

# Finds unused dependencies with cargo-udeps, on the pinned nightly
[group('code_check')]
udeps *args:
  cargo +{{udeps_toolchain}} udeps {{locked}} {{args}}

# Formats the Python helper scripts with black; `just fmt_py --check --diff` only checks them
[group('code_check')]
fmt_py *args:
  uvx black@{{black_version}} {{args}} scripts
