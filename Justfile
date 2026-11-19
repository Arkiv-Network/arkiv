import "./just/build.just"
import "./just/code_check.just"
import "./just/test.just"
import "./just/docker.just"
import "./just/kurtosis.just"

set shell := ["bash", "-euo", "pipefail", "-c"]

export RUST_BACKTRACE := env("RUST_BACKTRACE", "full")

# The cargo profile to build with. Empty means cargo's own default. 
# CI exports PROFILE=dev-fast
profile := env("PROFILE", "")

# GitHub Actions sets CI. There the lockfiles must already be up to date;
# locally, editing a dependency should simply update them.
_ci := env("CI", "")
locked := if _ci != "" { "--locked" } else { "" }

# The profile flag in cargo's spelling and in nextest's, and the directory the
# binaries land in.
_cargo_profile := if profile == "" { "" } else { "--profile " + profile }
_nextest_profile := if profile == "" { "" } else { "--cargo-profile " + profile }
bin_dir := "target" / if profile == "" { "debug" } else if profile == "dev" { "debug" } else { profile }

# Lists all the available commands
default:
  @just --list
