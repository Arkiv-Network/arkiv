# Runs the whole test suite: nextest, then the doc tests
[group('test')]
test: nextest test_doc

# Runs the tests with nextest; e.g. `just nextest -p arkiv-query` or `just nextest --no-run`
[group('test')]
nextest *args:
  cargo nextest run {{locked}} {{_nextest_profile}} {{args}}

# Runs the doc tests, which nextest does not run
[group('test')]
test_doc *args:
  cargo test {{locked}} {{_cargo_profile}} --doc {{args}}

# They drive the real binary across a restart, on GolemDB on disk, past reth's
# persistence threshold.
#
# Scoped to arkiv-reth: the workspace's other #[ignore]d test is a benchmark,
# run by hand.
#
# Runs arkiv-reth's slow restart tests, which are #[ignore]d by default
[group('test')]
test_slow *args:
  cargo nextest run {{locked}} {{_nextest_profile}} -p arkiv-reth --run-ignored only {{args}}
