# The prefill fixture's checks: crates/arkiv-prefill/fixtures/small, a full
# prefill whose block 0 root and hash are committed in its expected.json.
#
# Prefills the fixture with a built arkiv-reth, finalizes it, and checks block 0 against expected.json
[group('test')]
prefill_check binary=(bin_dir / "arkiv-reth"):
  scripts/prefill/check.sh "{{binary}}"

# Run it after a change meant to alter block 0, and commit the new file.
#
# Rewrites the prefill fixture's expected.json from a built arkiv-reth
[group('test')]
prefill_fixture_update binary=(bin_dir / "arkiv-reth"):
  scripts/prefill/check.sh "{{binary}}" --update
