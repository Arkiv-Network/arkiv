# Builds all artifacts
[group('build')]
build:
  cargo build --workspace --all-targets --all-features
