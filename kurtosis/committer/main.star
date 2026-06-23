# Adds the arkiv-committer service into the enclave, wired to the in-enclave
# sequencer EL + base chain by service DNS. Today it's a DA stub (self-checks the
# format and heartbeats); the real poll/encode/post loop drops into the same slot.
#
# Self-contained (no imports): kurtosis can't import a remote package from a local
# one, and up.py runs this from a neutral temp dir because kurtosis roots a local
# package's archive at the enclosing git repo root.

def run(plan, args):
    plan.add_service(
        name = "committer",
        config = ServiceConfig(
            image = "arkiv-committer:dev",
            cmd = [
                "--sequencer-rpc", "http://el-1-reth-lighthouse:8545",
                "--base-chain-rpc", "http://base-chain:8545",
            ],
        ),
    )
