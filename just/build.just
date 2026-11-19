# Builds the workspace: arkiv-reth, arkiv-cli and the library crates
[group('build')]
build *args:
  cargo build {{locked}} {{_cargo_profile}} {{args}}
