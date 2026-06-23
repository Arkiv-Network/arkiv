# Adds the bespoke reth "base chain" below the Arkiv chain — the DA / settlement
# substrate the committer posts to later. Plain reth, no Arkiv semantics, no CL
# (--dev auto-mines). Run into the same enclave as the Arkiv chain.
#
# Self-contained (no imports): kurtosis can't import a remote package from a
# local one, and up.sh runs this from a neutral temp dir because kurtosis roots
# a local package's archive at the enclosing git repo root.

def run(plan, args):
    plan.add_service(
        name = "base-chain",
        config = ServiceConfig(
            image = "ghcr.io/paradigmxyz/reth:v2.2.0",
            ports = {
                "rpc": PortSpec(
                    number = 8545,
                    transport_protocol = "TCP",
                    application_protocol = "http",
                ),
            },
            cmd = [
                "node",
                "--dev",
                "--dev.block-time", "2s",
                "--http",
                "--http.addr", "0.0.0.0",
                "--http.api", "eth,net,web3,txpool",
            ],
        ),
    )
